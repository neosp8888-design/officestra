import { AgentBusyError } from './agent-runtime.mjs';
import { withCharacterSessionLocks } from './character-settings.mjs';
import { normalizeLocalDefinition, localProfileReasoningOptions } from './local-provider-service.mjs';
import { isIP } from 'node:net';
import { WindowsLMStudioHost } from './local-provider-host.mjs';

export function assignedLocalDefinition(definition, assignment) {
  return normalizeLocalDefinition({
    ...definition,
    host:{...definition.host,...(assignment.address?{address:assignment.address}:{})},
    profile:{...definition.profile,...(assignment.reasoning?{reasoning:assignment.reasoning}:{})},
  });
}

export async function controlLocalModel({pool,runtime,localProviders,characterID,action}) {
  if(!runtime||runtime.draining)throw new AgentBusyError('백엔드가 준비된 뒤 다시 선택하세요.');
  if(!['start','stop'].includes(action))throw new Error('Unsupported model action');
  const result=await pool.query(`SELECT p.definition, c.config->>'localHostAddress' AS address,
    c.config->>'localReasoning' AS reasoning FROM characters c
    JOIN local_agent_profiles p ON p.id=c.config->>'localProfileId'
    WHERE c.id=$1 AND p.enabled=true`,[characterID]);
  const row=result.rows[0];
  if(!row)throw new Error('로컬 모델을 먼저 선택하세요.');
  return {ok:true,...await localProviders.controlModel(assignedLocalDefinition(row.definition,row),action)};
}

export function normalizeLocalHostAddress(value) {
  const address=typeof value==='string'?value.trim():'';
  if(isIP(address)!==4)throw new Error('올바른 IPv4 주소를 입력하세요. 포트나 http://는 넣지 마세요.');
  return address;
}

function localPermission(previousPermission, backend) {
  const fullAccess=['danger-full-access','dangerously-skip-permissions','bypassPermissions'].includes(previousPermission);
  if(fullAccess)return backend==='codex'?'danger-full-access':'bypassPermissions';
  const workspaceWrite=['workspace-write','accept-edits','acceptEdits','auto'].includes(previousPermission);
  if(workspaceWrite)return backend==='codex'?'workspace-write':'auto';
  return backend==='codex'?'read-only':'plan';
}

export async function setLocalHostAddress({pool,runtime,localProviders,characterID,address,verifyHost=async definition=>{
  const host=new WindowsLMStudioHost({...definition,stateDirectory:'/tmp/officestra-address-check'});
  try{await host.remote('Write-Output OFFICESTRA_ADDRESS_OK',{timeout:10000});}
  catch{throw new Error('해당 IP의 기존 PC에 SSH 연결할 수 없습니다. 주소와 PC 상태를 확인하세요.');}
},broadcast=()=>{}}) {
  address=normalizeLocalHostAddress(address);
  const busy=()=>!runtime||runtime.draining||runtime.running.has(characterID)||runtime.preparingCharacters.has(characterID)||runtime.compactingCharacters.has(characterID)||runtime.terminalSessionRegistry?.has(characterID);
  if(busy())throw new AgentBusyError('이 직원의 작업을 마치고 터미널을 닫은 뒤 전환하세요.');
  runtime.preparingCharacters.add(characterID);
  try {
    await withCharacterSessionLocks(pool,[characterID],async client=>{
      await client.query('BEGIN');
      try {
        const current=(await client.query('SELECT config FROM characters WHERE id=$1 FOR UPDATE',[characterID])).rows[0];
        const profileID=current?.config?.localProfileId;
        if(!profileID)throw new Error('Local profile required');
        const found=(await client.query('SELECT definition FROM local_agent_profiles WHERE id=$1 AND enabled=true FOR SHARE',[profileID])).rows[0];
        const definition=normalizeLocalDefinition(found?.definition);
        definition.host.address=address;
        await verifyHost(definition);
        if(runtime.draining||runtime.running.has(characterID)||runtime.compactingCharacters.has(characterID)||runtime.terminalSessionRegistry?.has(characterID))throw new AgentBusyError('이 직원의 작업을 마치고 터미널을 닫은 뒤 전환하세요.');
        await localProviders?.closeIdleProfile(profileID,address);
        await client.query('UPDATE characters SET config=$2::jsonb,updated_at=now() WHERE id=$1',[characterID,JSON.stringify({...current.config,localHostAddress:address})]);
        await client.query('COMMIT');
      }catch(error){await client.query('ROLLBACK');throw error;}
    });
    runtime.closeClaudeWorker(characterID);
    broadcast({type:'local.changed',characterId:characterID});
    return {ok:true,characterId:characterID,address};
  }finally{runtime.preparingCharacters.delete(characterID);}
}

export function localSelectionSettings(previous, definition) {
  const config={...(previous.config??{})};
  if(!definition) {
    if(!config.localProfileId)return null;
    const saved=config.localPreviousSettings;
    if(!saved)throw new Error('Previous cloud settings are unavailable');
    delete config.localProfileId;delete config.localPreviousSettings;delete config.localReasoning;delete config.localHostAddress;
    delete config.executablePath;
    if(saved.executablePath)config.executablePath=saved.executablePath;
    return {...saved,autoCompactPercent:saved.autoCompactPercent??90,config};
  }
  if(config.localProfileId===definition.profile.id)return null;
  const backend=definition.profile.backend;
  if(!['claude','codex'].includes(backend))throw new Error('Unsupported local runner backend');
  if(!config.localProfileId)config.localPreviousSettings={backend:previous.backend,model:previous.model,effort:previous.effort,fastMode:previous.fastMode,permission:previous.permission,autoCompactPercent:previous.autoCompactPercent??90,executablePath:config.executablePath??null};
  delete config.executablePath;
  const localAddress=config.localProfileId?config.localHostAddress:undefined;
  config.localProfileId=definition.profile.id;
  delete config.localReasoning;
  delete config.localHostAddress;
  // The runner changes, not the user's PC route. SSH still validates the new
  // profile's pinned host identity before any model request can be made.
  if(localAddress)config.localHostAddress=normalizeLocalHostAddress(localAddress);
  return {backend,model:definition.profile.model,effort:'default',fastMode:false,permission:localPermission(previous.permission,backend),autoCompactPercent:100,config};
}

export async function setLocalReasoning({pool,runtime,characterID,reasoning,broadcast=()=>{}}) {
  if(!['default','on','off','low','medium','xhigh'].includes(reasoning))throw new Error('Unsupported local reasoning option');
  const busy=()=>!runtime||runtime.draining||runtime.running.has(characterID)||runtime.preparingCharacters.has(characterID)||runtime.compactingCharacters.has(characterID)||runtime.terminalSessionRegistry?.has(characterID);
  if(busy())throw new AgentBusyError('이 직원의 작업을 마치고 터미널을 닫은 뒤 전환하세요.');
  runtime.preparingCharacters.add(characterID);
  try {
    await withCharacterSessionLocks(pool,[characterID],async client=>{
      await client.query('BEGIN');
      try {
        if(runtime.draining||runtime.running.has(characterID)||runtime.compactingCharacters.has(characterID)||runtime.terminalSessionRegistry?.has(characterID))throw new AgentBusyError('직원 상태가 바뀌었습니다. 작업 종료 후 다시 선택하세요.');
        const previous=(await client.query('SELECT config FROM characters WHERE id=$1 FOR UPDATE',[characterID])).rows[0];
        if(!previous?.config?.localProfileId)throw new Error('Local profile required');
        const found=(await client.query('SELECT definition FROM local_agent_profiles WHERE id=$1 AND enabled=true FOR SHARE',[previous.config.localProfileId])).rows[0];
        const allowed=localProfileReasoningOptions(normalizeLocalDefinition(found?.definition));
        if(!allowed.includes(reasoning))throw new Error('Unsupported local reasoning option for this runtime');
        await client.query('UPDATE characters SET config=$2::jsonb,updated_at=now() WHERE id=$1',[characterID,JSON.stringify({...previous.config,localReasoning:reasoning})]);
        await client.query('COMMIT');
      }catch(error){await client.query('ROLLBACK');throw error;}
    });
    runtime.closeClaudeWorker(characterID);
    broadcast({type:'local.changed',characterId:characterID});
    return {ok:true,characterId:characterID,reasoning};
  }finally{runtime.preparingCharacters.delete(characterID);}
}

export async function selectLocalProfile({pool,runtime,characterID,profileID,broadcast=()=>{}}) {
  if(!runtime||runtime.draining)throw new AgentBusyError('백엔드가 준비된 뒤 다시 선택하세요.');
  const busy=()=>runtime.running.has(characterID)||runtime.preparingCharacters.has(characterID)||runtime.compactingCharacters.has(characterID)||runtime.terminalSessionRegistry?.has(characterID);
  if(busy())throw new AgentBusyError('이 직원의 작업을 마치고 터미널을 닫은 뒤 전환하세요.');
  runtime.preparingCharacters.add(characterID);
  let plan,result;
  try {
    result=await withCharacterSessionLocks(pool,[characterID],async client=>{
      await client.query('BEGIN');
      try {
        if(runtime.draining||runtime.running.has(characterID)||runtime.compactingCharacters.has(characterID)||runtime.terminalSessionRegistry?.has(characterID))throw new AgentBusyError('직원 상태가 바뀌었습니다. 작업 종료 후 다시 선택하세요.');
        const current=await client.query('SELECT id,backend,model,effort,fast_mode AS "fastMode",permission,auto_compact_percent AS "autoCompactPercent",config FROM characters WHERE id=$1 FOR UPDATE',[characterID]);
        if(!current.rows[0])throw new Error('직원을 찾을 수 없습니다.');
        let definition=null;
        if(profileID) {
          const found=await client.query('SELECT definition FROM local_agent_profiles WHERE id=$1 AND enabled=true FOR SHARE',[profileID]);
          if(!found.rows[0])throw new Error('사용 가능한 로컬 모델이 없습니다.');
          definition=normalizeLocalDefinition(found.rows[0].definition);
        }
        const next=localSelectionSettings(current.rows[0],definition);
        if(next) {
          if(current.rows[0].config?.localProfileId)await runtime.localProviders?.closeIdleProfile(current.rows[0].config.localProfileId);
          plan=await runtime.inspectWorkspaceForSessionEnd(characterID,client);
          await runtime.applyWorkspaceSessionEndPlan(client,plan);
          await client.query('UPDATE characters SET backend=$2,model=$3,effort=$4,fast_mode=$5,permission=$6,config=$7::jsonb,auto_compact_percent=$8,updated_at=now() WHERE id=$1',[characterID,next.backend,next.model,next.effort,next.fastMode,next.permission,JSON.stringify(next.config),next.autoCompactPercent]);
        }
        await client.query('COMMIT');
        return {ok:true,characterId:characterID,localProfileId:profileID||null,newSession:!!next};
      }catch(error){await client.query('ROLLBACK');throw error;}
    });
    if(plan) {
      runtime.closeClaudeWorker(characterID);
      try { await runtime.finalizeWorkspaceSessionEndPlan(plan); }
      catch { result.warning='설정은 저장됐지만 이전 작업 공간 정리 상태를 확인해야 합니다.'; }
    }
    broadcast({type:'local.changed',characterId:characterID});
    broadcast({type:'session.changed',characterId:characterID});
    return result;
  }finally {runtime.preparingCharacters.delete(characterID);}
}
