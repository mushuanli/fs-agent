#!/usr/bin/env python3
"""Deterministic app-server peer: no model access or personal Codex state."""
import json
import copy
import os
import sys
import time

threads = {}
turn_count = 0
awaiting_turn = None

def emit(value):
    print(json.dumps(value), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    params = request.get('params', {})
    if method == 'initialized':
        continue
    if method is None:
        if awaiting_turn is not None:
            emit(awaiting_turn)
            awaiting_turn = None
        emit({'method': 'serverRequest/resolved', 'params': {'threadId': 'session-1', 'requestId': request['id']}})
        continue
    result = {}
    if method == 'thread/start':
        if params.get('sandbox') != ('danger-full-access' if os.environ.get('PI_AGENT_PROJECT') else 'workspace-write'):
            emit({'id': request['id'], 'error': {'code': -32602, 'message': 'sandbox'}})
            continue
        thread_id = 'session-' + str(len(threads) + 1)
        thread = {'id': thread_id, 'cwd': params['cwd'], 'name': '<script>history</script>', 'createdAt': 1791458500, 'updatedAt': 1791458565, 'status': {'type': 'idle'}, 'turns': []}
        threads[thread_id] = thread
        result = {'thread': thread}
    elif method == 'thread/list':
        result = {'data': [thread for thread in threads.values() if bool(thread.get('archived')) == bool(params.get('archived')) and (not params.get('ancestorThreadId') or thread.get('spawnedFromId') == params['ancestorThreadId'])], 'nextCursor': None}
    elif method in ('thread/read', 'thread/resume'):
        thread = threads.get(params['threadId'])
        if thread is None:
            thread = {'id': params['threadId'], 'cwd': '/unauthorized', 'status': {'type': 'idle'}, 'turns': []}
        result = {'thread': thread}
    elif method == 'thread/fork':
        original = threads[params['threadId']]
        thread_id = 'session-' + str(len(threads) + 1)
        thread = copy.deepcopy(original)
        thread.update(id=thread_id, cwd=params['cwd'], forkedFromId=original['id'])
        threads[thread_id] = thread
        result = {'thread': thread}
    elif method == 'thread/name/set':
        threads[params['threadId']]['name'] = params['name']
        if params['name'] == 'fixture:foreign-child':
            threads['foreign-child'] = {'id': 'foreign-child', 'cwd': '/unauthorized', 'status': {'type': 'notLoaded'}, 'archived': True, 'spawnedFromId': params['threadId']}
        emit({'method': 'thread/name/updated', 'params': {'threadId': params['threadId'], 'threadName': params['name']}})
    elif method in ('thread/archive', 'thread/unarchive'):
        thread = threads[params['threadId']]
        thread['archived'] = method == 'thread/archive'
        thread['status'] = {'type': 'notLoaded'} if thread['archived'] else {'type': 'idle'}
        result = {} if thread['archived'] else {'thread': thread}
        emit({'method': 'thread/archived' if thread['archived'] else 'thread/unarchived', 'params': {'threadId': thread['id']}})
    elif method == 'thread/delete':
        deleted = [params['threadId']] + [t['id'] for t in threads.values() if t.get('spawnedFromId') == params['threadId']]
        for thread_id in deleted:
            threads.pop(thread_id, None)
            emit({'method': 'thread/deleted', 'params': {'threadId': thread_id}})
    elif method == 'turn/start':
        turn_count += 1
        turn_id = 'turn-' + str(turn_count)
        result = {'turn': {'id': turn_id}}
        thread_id = params['threadId']
        threads[thread_id]['status'] = {'type': 'active'}
        if params['input'][0]['text'] == 'approval-before-start-reply':
            awaiting_turn = {'id': request['id'], 'result': result}
            emit({'id': 'approval-early', 'method': 'item/commandExecution/requestApproval', 'params': {'threadId': thread_id, 'turnId': turn_id, 'command': 'echo test'}})
            continue
        emit({'id': request['id'], 'result': result})
        emit({'method': 'turn/started', 'params': {'threadId': thread_id, 'turn': {'id': turn_id}}})
        emit({'method': 'item/agentMessage/delta', 'params': {'threadId': thread_id, 'turnId': turn_id, 'delta': '<script>output</script>'}})
        emit({'id': 'approval-1', 'method': 'item/commandExecution/requestApproval', 'params': {'threadId': thread_id, 'turnId': turn_id, 'command': 'echo test'}})
        continue
    elif method == 'turn/interrupt':
        threads[params['threadId']]['status'] = {'type': 'idle'}
        emit({'method': 'turn/completed', 'params': {'threadId': params['threadId'], 'turn': {'id': params['turnId'], 'status': 'interrupted'}}})
    elif method == 'fixture/delay':
        time.sleep(0.1)
    emit({'id': request['id'], 'result': result})
