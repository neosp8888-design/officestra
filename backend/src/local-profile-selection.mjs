import { AgentBusyError } from './agent-runtime.mjs';
import { withCharacterSessionLocks } from './character-settings.mjs';
import { normalizeLocalDefinition } from './local-provider-service.mjs';

export function localSelectionSettings(previous, definition) {
  const config={...(previous.config??{})};
  if(!definition) {
    if(!config.localProfileId)return null;
    const saved=config.localPreviousSettings;
    if(!saved)throw new Error('Previous cloud settings are unavailable');
    delete config.localProfileId;delete config.localPreviousSettings;
    delete config.executablePath;
    if(saved.executablePath)config.executablePath=saved.executablePath;
    return {...saved,config};
  }
  if(config.localProfileId===definition.profile.id)return null;
  if(!config.localProfileId)config.localPreviousSettings={backend:previous.backend,model:previous.model,effort:previous.effort,fastMode:previous.fastMode,permission:previous.permission,executablePath:config.executablePath??null};
  delete config.executablePath;
  config.localProfileId=definition.profile.id;
  const permission=['danger-full-access','dangerously-skip-permissions','bypassPermissions'].includes(previous.permission)?'bypassPermissions'
    :['workspace-write','accept-edits','acceptEdits','auto'].includes(previous.permission)?'auto':'plan';
  return {backend:'claude',model:definition.profile.model,effort:'default',fastMode:false,permission,config};
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
        const current=await client.query('SELECT id,backend,model,effort,fast_mode AS "fastMode",permission,config FROM characters WHERE id=$1 FOR UPDATE',[characterID]);
        if(!current.rows[0])throw new Error('직원을 찾을 수 없습니다.');
        let definition=null;
        if(profileID) {
          const found=await client.query('SELECT definition FROM local_agent_profiles WHERE id=$1 AND enabled=true FOR SHARE',[profileID]);
          if(!found.rows[0])throw new Error('사용 가능한 로컬 모델이 없습니다.');
          definition=normalizeLocalDefinition(found.rows[0].definition);
        }
        const next=localSelectionSettings(current.rows[0],definition);
        if(next) {
          plan=await runtime.inspectWorkspaceForSessionEnd(characterID,client);
          await runtime.applyWorkspaceSessionEndPlan(client,plan);
          await client.query('UPDATE characters SET backend=$2,model=$3,effort=$4,fast_mode=$5,permission=$6,config=$7::jsonb,updated_at=now() WHERE id=$1',[characterID,next.backend,next.model,next.effort,next.fastMode,next.permission,JSON.stringify(next.config)]);
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
