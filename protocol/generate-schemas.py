#!/usr/bin/env python3
"""Regenerate v1 parameter schemas and representative request fixtures.

Maintain these declarations alongside typed Rust handlers. The contract test
checks every fixture and all allowed/required parameter names against this file's
output. Additional semantic restrictions remain in the Rust handlers.
"""
import json
from pathlib import Path
ROOT = Path(__file__).resolve().parent / 'v1'
def obj(properties=None, required=()):
    return {'type':'object','additionalProperties':False,'properties':properties or {},'required':list(required)}
def arr(items, **kw):return {'type':'array','items':items,**kw}
def ref(name):return {'$ref':'#/$defs/'+name}
def uint(minimum=0):return {'type':'integer','minimum':minimum,'maximum':18446744073709551615}
UUID={'type':'string','format':'uuid','pattern':'^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'}
STRING={'type':'string'};BOOL={'type':'boolean'};TEXT={'type':'string','minLength':1}
OID=obj({'algorithm':{'const':'sha1'},'hex':{'type':'string','pattern':'^[0-9a-f]{40}$'}},['algorithm','hex'])
COMMIT=obj({'name':TEXT,'email':TEXT,'message':{'type':'string','maxLength':65536}},['name','email','message'])
BINDING_PROPERTIES={'identity':TEXT,'mode':{'enum':['central','worker']},'binding_generation':uint(1),'object_format':{'const':'sha1'},'allowed_refs':arr(STRING),'allowed_targets':arr(STRING),'allowed_target_prefixes':arr({'type':'string','pattern':'^refs/heads/.+/$'}),'remote_url':{'type':['string','null']},'credential_profile':{'type':['string','null']},'allow_file_transport':BOOL,'feature_profile':{}}
binding=obj(BINDING_PROPERTIES,['identity','mode'])
fullbinding=obj(dict(BINDING_PROPERTIES,repository_id=ref('uuid')),['repository_id','identity','mode','binding_generation','object_format'])
defs={'uuid':UUID,'oid':OID,'commit':COMMIT,'binding':binding,'replacement_binding':fullbinding,'empty':obj()}
ops={}
def add(name,schema):key=name.replace('.','_').replace('-','_');defs[key]=schema;ops[name]=key
for op in ['capabilities','service.metrics','doctor','repo.show']:add(op,obj())
add('repo.register',{'oneOf':[ref('binding'),obj({'binding':ref('binding')},['binding'])]})
add('repo.binding-update',obj({'binding':ref('replacement_binding'),'reason':TEXT},['binding','reason']))
add('repo.sync',obj({'refs':arr(STRING)}))
add('repo.observation',obj({'refs':arr(STRING),'max_age_ms':{'type':'integer'}}))
add('inspect',obj({'oid':ref('oid')},['oid']))
add('tree',obj({'oid':ref('oid'),'limit':uint(),'recursive':BOOL},['oid']))
add('diff',obj({'base':ref('oid'),'target':ref('oid'),'limit':uint()},['base','target']))
add('contains',obj({'ancestor':ref('oid'),'descendant':ref('oid')},['ancestor','descendant']))
item={'oneOf':[obj({'operation':{'const':op},'params':ref(ops[op])},['operation','params']) for op in ['inspect','tree','diff','contains']]}
add('query.batch',obj({'items':arr(item,maxItems=100)},['items']))
add('workspace.create',obj({'base':ref('oid')},['base']))
add('workspace.inspect',obj({'workspace_id':ref('uuid')},['workspace_id']))
checks={'workspace_id':ref('uuid'),'expected_generation':uint(1),'expected_head':ref('oid'),'quiescent':{'const':True}}
add('workspace.checkpoint',obj(dict(checks,mode={'enum':['head','all']},commit={'anyOf':[ref('commit'),{'type':'null'}]}),checks))
add('workspace.retire',obj(checks,checks))
add('artifact.export',obj({'source':ref('oid'),'base':ref('oid'),'retention_handle':ref('uuid'),'confirmed_prerequisites':arr(ref('oid')),'reference_only':BOOL,'caller_correlation':{}},['source','base','retention_handle']))
add('artifact.import',obj({'artifact_handle':ref('uuid')},['artifact_handle']))
add('candidate.create',obj({'base':ref('oid'),'sources':arr(obj({'oid':ref('oid'),'retention_handle':ref('uuid')},['oid','retention_handle']),maxItems=64),'commit':ref('commit'),'merge_profile':{'const':'git-ort-v1'},'supersedes':{'anyOf':[ref('uuid'),{'type':'null'}]}},['base','sources','commit']))
add('candidate.show',obj({'candidate_id':ref('uuid')},['candidate_id']))
add('publish',obj({'candidate_id':ref('uuid'),'target':{'type':'string','pattern':'^refs/heads/'},'expected_old':dict(ref('oid'),description='Exact old commit; forty zeroes requires the target branch to be absent.'),'update':{'enum':['fast_forward','replace'],'description':'replace is limited to allowed_target_prefixes branches and swaps the branch under the exact expected_old lease.'}},['candidate_id','target','expected_old']))
add('operation.show',obj({'target_operation_id':ref('uuid')},['target_operation_id']))
target={'target_operation_id':ref('uuid'),'expected_generation':uint(1)}
add('operation.reconcile',obj(target,target))
evidence={'oneOf':[TEXT,{'type':'object','minProperties':1}]}
resolve=dict(target,disposition={'enum':['abandon','permit_new_operation']},reason=TEXT,old_publishers_stopped={'const':True},evidence=evidence)
add('operation.resolve',obj(resolve,resolve))
add('retention.inspect',obj({'handle_id':{'anyOf':[ref('uuid'),{'type':'null'}]},'after':{'anyOf':[ref('uuid'),{'type':'null'}]},'limit':uint(1)}))
retref=obj({'name':STRING,'oid':{'type':'string','pattern':'^[0-9a-f]{40}$'}},['name','oid'])
add('retention.release',obj({'handle_id':ref('uuid'),'expected_generation':uint(1),'refs':arr(retref),'reason':TEXT},['handle_id','expected_generation','refs','reason']))
add('repo.cache-evict',obj({'expected_storage_generation':uint(1),'reconstructible':{'const':True},'reason':TEXT},['expected_storage_generation','reconstructible','reason']))
add('maintenance',obj({'kind':{'enum':['indexes','repack',None]}}))
clear={'expected_incarnation':ref('uuid'),'reason':TEXT,'evidence':evidence,'old_environment_stopped':{'const':True}}
add('recovery.clear-fence',obj(clear,clear))
operations={'$schema':'https://json-schema.org/draft/2020-12/schema','title':'aq-git v1 operation parameters','$defs':defs}
(ROOT/'operations.schema.json').write_text(json.dumps(operations,indent=2)+'\n')
request=json.loads((ROOT/'request.schema.json').read_text())
request['allOf']=[rule for rule in request['allOf'] if 'properties' not in rule.get('then',{}) or 'params' not in rule['then']['properties']]
for op,key in ops.items():
    then={'properties':{'params':{'$ref':f'operations.schema.json#/$defs/{key}'}}}
    if defs[key].get('required') or 'oneOf' in defs[key]:then['required']=['params']
    request['allOf'].append({'if':{'properties':{'operation':{'const':op}},'required':['operation']},'then':then})
(ROOT/'request.schema.json').write_text(json.dumps(request,indent=2)+'\n')
# Literal deterministic fake IDs/OIDs; these are wire examples, not live resources.
uid='11111111-1111-4111-8111-111111111111';handle='22222222-2222-4222-8222-222222222222';operation='33333333-3333-4333-8333-333333333333'
oid={'algorithm':'sha1','hex':'a'*40};other={'algorithm':'sha1','hex':'b'*40};commit={'name':'Fixture','email':'fixture@example.test','message':'Explicit metadata'}
params={op:{} for op in ops}
params.update({'repo.register':{'binding':{'identity':'fixture','mode':'worker','binding_generation':1}},'repo.binding-update':{'binding':dict(identity='fixture',mode='worker',repository_id=uid,binding_generation=2,object_format='sha1'),'reason':'adopt caller generation'},'inspect':{'oid':oid},'tree':{'oid':oid,'limit':100},'diff':{'base':oid,'target':other,'limit':100},'contains':{'ancestor':oid,'descendant':other},'query.batch':{'items':[{'operation':'contains','params':{'ancestor':oid,'descendant':other}}]},'workspace.create':{'base':oid},'workspace.inspect':{'workspace_id':handle},'workspace.checkpoint':{'workspace_id':handle,'expected_generation':1,'expected_head':oid,'quiescent':True,'mode':'all','commit':commit},'workspace.retire':{'workspace_id':handle,'expected_generation':2,'expected_head':oid,'quiescent':True},'artifact.export':{'source':other,'base':oid,'retention_handle':handle},'artifact.import':{'artifact_handle':handle},'candidate.create':{'base':oid,'sources':[{'oid':other,'retention_handle':handle}],'commit':commit},'candidate.show':{'candidate_id':handle},'publish':{'candidate_id':handle,'target':'refs/heads/main','expected_old':oid},'operation.show':{'target_operation_id':handle},'operation.reconcile':dict(target_operation_id=handle,expected_generation=1),'operation.resolve':dict(target_operation_id=handle,expected_generation=1,disposition='abandon',reason='old publisher stopped',old_publishers_stopped=True,evidence={'fixture':'process cleanup complete'}),'retention.release':dict(handle_id=handle,expected_generation=1,refs=[{'name':f'refs/aq-git/checkpoints/{handle}','oid':'a'*40}],reason='handoff acknowledged'),'repo.cache-evict':dict(expected_storage_generation=1,reconstructible=True,reason='rehydratable cache'),'recovery.clear-fence':dict(expected_incarnation=handle,reason='environment stopped',evidence={'fixture':'cleanup complete'},old_environment_stopped=True)})
mutations={'repo.register','repo.binding-update','repo.sync','repo.cache-evict','workspace.create','workspace.checkpoint','workspace.retire','artifact.export','artifact.import','candidate.create','publish','operation.reconcile','operation.resolve','retention.release','maintenance','recovery.clear-fence'}
rootops={'capabilities','service.metrics','doctor','recovery.clear-fence'}
for op in ops:
    envelope=dict(protocol_version=1,request_id=op+'-example',operation=op,params=params[op])
    if op not in rootops:envelope['repository_id']=uid
    if op in mutations:
        envelope['operation_id']=operation
        if op not in rootops:envelope['expected_binding_generation']=0 if op=='repo.register' else 1
    name=op.replace('.','-')
    (ROOT/'examples'/f'{name}.request.json').write_text(json.dumps(envelope,indent=2)+'\n')
print(f'Generated schemas and {len(ops)} request examples')
# Representative decoded responses use the same deterministic fixture identities.
at='2026-10-30T00:00:00Z'
binding_result=dict(repository_id=uid,identity='fixture',mode='worker',binding_generation=1,object_format='sha1',allowed_refs=[],allowed_targets=[],remote_url=None,credential_profile=None,allow_file_transport=False,feature_profile=None)
observation=dict(observation_id=handle,binding_generation=1,refs={'refs/heads/main':'a'*40},started_at=at,completed_at=at,started_at_ms=1793318400000,completed_at_ms=1793318400000,covered_refs=['refs/heads/main'],last_refresh_error=None)
workspace=dict(workspace_id=handle,repository_id=uid,binding_generation=1,generation=1,base='a'*40,path=f'/runtime/workspaces/{handle}',lifecycle='active',allocating_operation=handle,last_checkpoint=None,retiring_operation=None,config_digest='c'*64)
retention=dict(version=1,id=handle,repository_id=uid,binding_generation=1,generation=1,resource='checkpoint',refs=[{'name':f'refs/aq-git/checkpoints/{handle}','oid':'a'*40}],state='active',dependencies=[],release_operation=None,release_reason=None)
manifest=dict(format_version=1,repository_id=uid,repository_identity='fixture',binding_generation=1,transfer_id=handle,object_format='sha1',base=oid,source=other,prerequisites=[],bundle_size=128,bundle_sha256='c'*64,producer_installation_id=operation,reference_only=False,caller_correlation={})
candidate=dict(candidate_id=handle,state='ready',base=oid,sources=[{'oid':other,'retention_handle':handle}],merge_profile='git-ort-v1',git_version='git version 2.43.0',commit=commit,supersedes=None,candidate=other,clean_prefix=other,conflict_step=None,conflicting_paths=[],conflict_paths_truncated=False,conflict_path_count=0,retention_handle=handle)
results={
 'capabilities':dict(protocol_version=1,state_format_version=1,operations=list(ops),performance_acceptance='not_certified'),
 'service.metrics':dict(git=dict(spawns=0,active=0,duration_micros=0,output_bytes=0,timeouts=0,output_limits=0),queries=dict(cache_bytes=0,cache_entries=0,object_exchanges=0)),
 'doctor':dict(state_format=1,inspected_operations=0,inspection_limit=1000,findings=[],process_incarnation=None),
 'repo.register':binding_result,'repo.show':binding_result,'repo.binding-update':dict(binding_result,binding_generation=2),
 'repo.sync':observation,'repo.observation':observation,
 'inspect':dict(oid=oid,tree=other,parents=[],author={'encoding':'utf8','value':'Fixture <fixture@example.test> 1700000000 +0000'},committer={'encoding':'utf8','value':'Fixture <fixture@example.test> 1700000000 +0000'},message={'encoding':'utf8','value':'Explicit metadata\n'}),
 'tree':dict(entries=[dict(mode='100644',type='blob',oid=other,path={'encoding':'utf8','value':'hello'})],truncated=False),
 'diff':dict(entries=[dict(status='M',path={'encoding':'utf8','value':'hello'})],truncated=False),
 'contains':dict(contains=False,semantics='strict_commit_ancestry_v1'),
 'query.batch':dict(items=[dict(ok=True,result=dict(contains=False,semantics='strict_commit_ancestry_v1'))]),
 'workspace.create':workspace,'workspace.inspect':dict(workspace=workspace,head=oid,clean=True,status_encoding='hex',status=''),
 'workspace.checkpoint':dict(workspace_id=handle,workspace_generation=2,source=other,retention_handle=handle,unchanged=False),
 'workspace.retire':dict(workspace,lifecycle='retired',generation=3,retiring_operation=operation,last_checkpoint=handle),
 'artifact.export':dict(artifact_handle=handle,manifest=manifest,retention_handle=handle),
 'artifact.import':dict(receiver_installation_id=operation,repository_id=uid,binding_generation=1,transfer_id=handle,source=other,retention_handle=handle,manifest_digest='d'*64,accepted_at=at,retention_state='active',historical=False),
 'candidate.create':candidate,'candidate.show':candidate,
 'publish':dict(outcome='applied',target='refs/heads/main',old=oid,new=other,acknowledged_at=at),
 'operation.show':dict(version=1,id=handle,payload={'operation':'publish'},payload_hash='d'*64,generation=3,status='complete',result=dict(outcome='already_present',observed_target=other,observed_at=at,caused_by_this_operation=False),error=None,created_at_ms=1793318400000,phase='terminal',metadata={},created_at=at,updated_at=at),
 'operation.reconcile':dict(target_operation_id=handle,resolution=dict(outcome='already_present',observed_target=other,observed_at=at,caused_by_this_operation=False)),
 'operation.resolve':dict(target_operation_id=handle,resolution=dict(outcome='resolved',disposition='abandon',reason='old publishers stopped',evidence='fixture cleanup complete',resolved_at=at,historical_remote_effect='unknown')),
 'retention.inspect':dict(handles=[retention],next_after=None),'retention.release':dict(retention,state='released',generation=3,release_operation=operation,release_reason='handoff acknowledged'),
 'repo.cache-evict':dict(format_version=1,storage_generation=2,lifecycle='absent',operation_id=operation),
 'maintenance':dict(kind='indexes',pruned=False),
 'recovery.clear-fence':dict(cleared_incarnation=handle,local_cleanup_attested=True,remote_effects_resolved=False),
}
for op,result in results.items():
    response=dict(protocol_version=1,request_id=op+'-example',ok=True,result=result)
    if op in mutations:response.update(operation_id=operation,operation_state='complete')
    (ROOT/'examples'/f'{op.replace(".","-")}.response.json').write_text(json.dumps(response,indent=2)+'\n')
errors=['unsupported_protocol','unsupported_feature','state_busy','state_incompatible','binding_conflict','idempotency_conflict','object_unavailable','observation_stale','prerequisite_missing','artifact_invalid','workspace_dirty','workspace_changed','conflicted','target_moved','operation_indeterminate','limit_exceeded','deadline_exceeded','overloaded','io_error','recovery_blocked','binding_in_use','retention_in_use','retention_released']
error_dir=ROOT/'examples/errors';error_dir.mkdir(exist_ok=True)
for code in errors:
    retry='safe_same_operation' if code in ['overloaded','state_busy','io_error','state_io','deadline_exceeded'] else 'refresh_inputs' if code in ['binding_conflict','target_moved','workspace_changed','object_unavailable','observation_stale','prerequisite_missing','generation_conflict'] else 'reconcile_first' if code=='operation_indeterminate' else 'manual'
    response=dict(protocol_version=1,request_id='error-example',ok=False,error=dict(code=code,message=code.replace('_',' ').capitalize(),details={},retry=retry))
    (error_dir/f'{code}.json').write_text(json.dumps(response,indent=2)+'\n')

# D0 keeps the v1 shapes; these examples define named profiles and absent leases.
for name, operation_name, parameters in [
    ('repo-register-authenticated', 'repo.register', {'identity':'fixture','mode':'central','remote_url':'https://github.com/example/project.git','credential_profile':'github-project','allowed_refs':['refs/heads/main'],'allowed_targets':['refs/heads/main','refs/heads/new']}),
    ('publish-new-branch', 'publish', {'candidate_id':handle,'target':'refs/heads/new','expected_old':{'algorithm':'sha1','hex':'0'*40}}),
    # Pull-request head namespaces: create or replace refs/heads/aq/* under an exact lease.
    ('repo-register-target-prefixes', 'repo.register', {'identity':'fixture','mode':'central','remote_url':'https://github.com/example/project.git','credential_profile':'github-project','allowed_refs':['refs/heads/main'],'allowed_targets':['refs/heads/main'],'allowed_target_prefixes':['refs/heads/aq/']}),
    ('publish-replace-head', 'publish', {'candidate_id':handle,'target':'refs/heads/aq/'+handle,'expected_old':{'algorithm':'sha1','hex':'a'*40},'update':'replace'}),
]:
    request = dict(protocol_version=1, request_id=name+'-example', operation=operation_name, repository_id=uid, operation_id=operation, expected_binding_generation=0 if operation_name=='repo.register' else 1, params=parameters)
    (ROOT/'examples'/f'{name}.request.json').write_text(json.dumps(request, indent=2)+'\n')
for dispatched in (False, True):
    response = dict(protocol_version=1,request_id='remote-auth-example',ok=False,operation_id=operation,operation_state='pending' if dispatched else 'failed',error=dict(code='remote_auth_failed',message='Remote authentication or host verification failed',details={'remote_effect_unknown':True,'dispatched':True} if dispatched else None,retry='reconcile_first' if dispatched else 'manual'))
    (ROOT/'examples/errors'/f'remote-auth-{"dispatched" if dispatched else "preflight"}.json').write_text(json.dumps(response,indent=2)+'\n')
