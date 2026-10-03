use super::{model::*, policy, service::SyncService, store::metadata as m};
use rusqlite::Connection;
use serde_json::{json, Value};

pub(super) fn text<'a>(body: &'a Value, key: &str) -> Result<&'a str> {
    body[key]
        .as_str()
        .ok_or_else(|| Error::new("INVALID_COMMAND", 400))
}
pub(super) fn operation_identity(body: &Value) -> Result<OperationIdentity> {
    Ok(OperationIdentity {
        operation_id: text(body, "operationId")?.into(),
        replica_id: text(body, "replicaId")?.into(),
        op_seq: text(body, "opSeq")?.into(),
        history_epoch: text(body, "historyEpoch")?.into(),
    })
}
fn op_key(replica: &str, seq: u64) -> String {
    format!("{replica}/{seq:020}")
}
struct Request<'a> {
    identity: OperationIdentity,
    replica: &'a str,
    seq: u64,
    key: String,
    hash: String,
    body: &'a Value,
    target: &'a str,
}
impl Request<'_> {
    fn decorate(&self, mut receipt: Value) -> Value {
        receipt["operation"] = json!(self.identity);
        receipt
    }
    fn record(&self, receipt: Value, finished: Option<u64>) -> Value {
        let receipt = self.decorate(receipt);
        json!({"requestHash":self.hash,"body":self.body,"target":self.target,"receipt":receipt,"finishedAt":finished})
    }
}
impl SyncService {
    pub fn command(&self, target: &str, body: &Value, cancel: bool) -> Result<Value> {
        self.command_inner(target, body, cancel)
    }
    fn command_inner(&self, target: &str, body: &Value, cancel: bool) -> Result<Value> {
        self.with_write(|db| {
            let request = self.request(target, body)?;
            if let Some(receipt) = self.admit(db, &request)? {
                return Ok(receipt);
            }
            super::fault::point("after-admission");
            self.execute_command(db, &request, cancel)
        })
    }
    fn admit(&self, db: &mut Connection, request: &Request<'_>) -> Result<Option<Value>> {
        let tx = db.transaction()?;
        if let Some(record) = m::get::<Value>(&tx, self.epoch(), "operation", &request.key)? {
            if record["requestHash"] != request.hash {
                return Err(Error::new("OPERATION_REUSED", 409));
            }
            return Ok(Some(request.decorate(record["receipt"].clone())));
        }
        let mut device: Replica = m::require(&tx, self.epoch(), "replica", request.replica)?;
        self.validate_admission(&tx, &device, request)?;
        self.quota(&tx, 0)?;
        self.capacity_for(&tx, 2)?;
        device.last_admitted_seq = request.seq;
        device.last_seen = self.time();
        m::put(&tx, self.epoch(), "replica", request.replica, &device)?;
        m::put(
            &tx,
            self.epoch(),
            "operation-id",
            text(request.body, "operationId")?,
            &json!(request.key),
        )?;
        m::put(
            &tx,
            self.epoch(),
            "operation",
            &request.key,
            &request.record(json!({"outcome":"unknown","state":"pending"}), None),
        )?;
        self.commit(tx)?;
        Ok(None)
    }
    fn request<'a>(&self, target: &'a str, body: &'a Value) -> Result<Request<'a>> {
        if text(body, "historyEpoch")? != self.epoch()
            || text(body, "authorityId")? != self.identity.authority_id
        {
            return Err(Error::new("HISTORY_EPOCH_CHANGED", 409));
        }
        let replica = text(body, "replicaId")?;
        policy::id(replica)?;
        policy::id(text(body, "operationId")?)?;
        let seq = policy::number(text(body, "opSeq")?)?;
        if serde_json::to_vec(body)?.len() > 512 * 1024 {
            return Err(Error::new("LIMIT_EXCEEDED", 413));
        }
        Ok(Request {
            identity: operation_identity(body)?,
            replica,
            seq,
            key: op_key(replica, seq),
            hash: digest(&serde_json::to_vec(&json!({"target":target,"body":body}))?),
            body,
            target,
        })
    }
    fn execute_command(
        &self,
        db: &mut Connection,
        request: &Request<'_>,
        cancel: bool,
    ) -> Result<Value> {
        let tx = db.transaction()?;
        tx.execute_batch("SAVEPOINT command")?;
        let result = if cancel {
            Err(Error::new("CANCELLED", 409))
        } else {
            self.apply(&tx, request.target, request.body).and_then(|v| {
                self.capacity_for(&tx, 0)?;
                Ok(v)
            })
        };
        self.validate_transaction_result(&tx, &result)?;
        let receipt = request.decorate(self.finalize_result(&tx, result)?);
        self.save_receipt(&tx, request, &receipt)?;
        super::fault::point("before-publish-commit");
        self.commit(tx)?;
        super::fault::point("after-publish-commit");
        if let Err(error) = super::fault::io("publish-commit-unknown") {
            self.mark_uncertain();
            return Err(error);
        }
        Ok(receipt)
    }
    fn validate_transaction_result(&self, db: &Connection, result: &Result<Value>) -> Result<()> {
        if let Some(error) = result.as_ref().err().filter(|e| e.unknown) {
            self.mark_uncertain();
            if !db.is_autocommit() {
                db.execute_batch("ROLLBACK").map_err(|_| Error::storage())?;
            }
            return Err(error.clone());
        }
        if db.is_autocommit() {
            self.mark_uncertain();
            return Err(Error::storage());
        }
        Ok(())
    }
    fn save_receipt(&self, db: &Connection, request: &Request<'_>, receipt: &Value) -> Result<()> {
        super::fault::io("before-receipt-write")?;
        m::put(
            db,
            self.epoch(),
            "operation",
            &request.key,
            &request.record(receipt.clone(), Some(self.time())),
        )
    }
    fn finalize_result(&self, db: &Connection, result: Result<Value>) -> Result<Value> {
        match result {
            Ok(value) => {
                db.execute_batch("RELEASE command")?;
                Ok(json!({"outcome":"committed","result":value}))
            }
            Err(e) if !e.unknown => {
                super::fault::io("before-savepoint-rollback")?;
                db.execute_batch("ROLLBACK TO command; RELEASE command")?;
                Ok(json!({"outcome":"not-committed","code":e.code,"status":e.status}))
            }
            Err(e) => {
                self.mark_uncertain();
                Err(e)
            }
        }
    }
    fn validate_admission(&self, db: &Connection, device: &Replica, r: &Request<'_>) -> Result<()> {
        validate_replica_sequence(
            device,
            r.seq,
            self.time(),
            self.config.replica_expiry_seconds,
        )?;
        let prior = m::get::<Value>(
            db,
            self.epoch(),
            "operation",
            &op_key(r.replica, r.seq.saturating_sub(1)),
        )?;
        if prior.is_some_and(|v| v["receipt"]["state"] == "pending") {
            return Err(Error::new("OPERATION_PENDING", 409));
        }
        if m::get::<Value>(
            db,
            self.epoch(),
            "operation-id",
            text(r.body, "operationId")?,
        )?
        .is_some()
        {
            return Err(Error::new("OPERATION_REUSED", 409));
        }
        Ok(())
    }
    pub fn operation(&self, replica: &str, seq: &str) -> Result<Value> {
        self.with_read(|db| {
            policy::id(replica)?;
            let seq = policy::number(seq)?;
            if let Some(record) =
                m::get::<Value>(db, self.epoch(), "operation", &op_key(replica, seq))?
            {
                let request = self.request(text(&record, "target")?, &record["body"])?;
                return Ok(request.decorate(record["receipt"].clone()));
            }
            let device: Replica = m::require(db, self.epoch(), "replica", replica)?;
            Err(Error::new(
                if seq <= device.last_admitted_seq {
                    "OPERATION_EXPIRED"
                } else {
                    "NOT_FOUND"
                },
                if seq <= device.last_admitted_seq {
                    410
                } else {
                    404
                },
            ))
        })
    }
    pub fn cancel_operation(&self, replica: &str, seq: &str, body: &Value) -> Result<Value> {
        match self.operation(replica, seq) {
            Ok(receipt) => return Ok(receipt),
            Err(error) if error.code == "NOT_FOUND" => {}
            Err(error) => return Err(error),
        }
        let original = &body["command"];
        let target = text(body, "target")?;
        if text(original, "replicaId")? != replica || text(original, "opSeq")? != seq {
            return Err(Error::new("INVALID_COMMAND", 400));
        }
        self.command(target, original, true)
    }
    pub fn register(&self, body: &Value) -> Result<Value> {
        self.with_write(|db| {
            let replica = text(body, "replicaId")?;
            policy::id(replica)?;
            let tx = db.transaction()?;
            if let Some(device) = m::get::<Replica>(&tx, self.epoch(), "replica", replica)? {
                return Ok(replica_value(
                    &device,
                    self.config.replica_expiry_seconds,
                    self.time(),
                ));
            }
            if m::count(&tx, self.epoch(), "replica")? >= self.config.max_replicas {
                return Err(Error::new("LIMIT_EXCEEDED", 429));
            }
            self.quota(&tx, 0)?;
            let device = Replica {
                replica_id: replica.into(),
                state: "reconciling".into(),
                last_admitted_seq: 0,
                last_seen: self.time(),
            };
            m::put(&tx, self.epoch(), "replica", replica, &device)?;
            self.commit(tx)?;
            Ok(replica_value(
                &device,
                self.config.replica_expiry_seconds,
                self.time(),
            ))
        })
    }
    pub fn replica(&self, replica: &str) -> Result<Value> {
        self.with_read(|db| {
            policy::id(replica)?;
            let device: Replica = m::require(db, self.epoch(), "replica", replica)?;
            Ok(replica_value(
                &device,
                self.config.replica_expiry_seconds,
                self.time(),
            ))
        })
    }
    fn validate_scopes(&self, db: &Connection, body: &Value) -> Result<()> {
        let scopes = body["scopes"]
            .as_array()
            .ok_or_else(|| Error::new("INVALID_COMMAND", 400))?;
        if scopes.len() > self.config.max_projects {
            return Err(Error::new("LIMIT_EXCEEDED", 429));
        }
        if scopes.is_empty() && m::count(db, "", "project")? > 0 {
            return Err(Error::new("RECONCILIATION_REQUIRED", 409));
        }
        for scope in scopes {
            self.decode_cursor(text(scope, "cursor")?, text(scope, "projectId")?, "catalog")?;
        }
        Ok(())
    }
    pub fn activate(&self, replica: &str, body: &Value) -> Result<Value> {
        self.with_write(|db| {
            policy::id(replica)?;
            let tx = db.transaction()?;
            let mut device: Replica = m::require(&tx, self.epoch(), "replica", replica)?;
            if device.state != "reconciling" {
                return Err(Error::new("REPLICA_EXPIRED", 410));
            }
            self.validate_scopes(&tx, body)?;
            device.state = "active".into();
            device.last_seen = self.time();
            m::put(&tx, self.epoch(), "replica", replica, &device)?;
            self.commit(tx)?;
            Ok(replica_value(
                &device,
                self.config.replica_expiry_seconds,
                self.time(),
            ))
        })
    }
}
fn replica_value(replica: &Replica, expiry: u64, time: u64) -> Value {
    json!({"replicaId":replica.replica_id,"state":if replica.state=="active" && !policy::replica_active(replica,time,expiry){"expired"}else{&replica.state},
        "lastAdmittedSeq":replica.last_admitted_seq.to_string()})
}

pub(super) fn version_key(id: &str, generation: &str) -> Result<String> {
    Ok(format!("{id}/{:020}", policy::number(generation)?))
}

fn validate_replica_sequence(device: &Replica, seq: u64, time: u64, expiry: u64) -> Result<()> {
    if seq <= device.last_admitted_seq {
        return Err(Error::new("OPERATION_EXPIRED", 410));
    }
    if !policy::replica_active(device, time, expiry) {
        return Err(Error::new("REPLICA_EXPIRED", 410));
    }
    if seq != device.last_admitted_seq + 1 {
        return Err(Error::new("OPERATION_SEQUENCE", 409));
    }
    Ok(())
}
