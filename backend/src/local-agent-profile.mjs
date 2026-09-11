// Opt-in local launch specification. No server, tunnel, worker, or DB mutation.
// The caller must provision an authenticated loopback compatibility bridge and
// persist the provider snapshot per turn before enabling this for an employee.
import { createHash } from 'node:crypto';
import { buildArguments, claudePersistentArguments, executionEnvironment, locateExecutable,
  claudePersistentWorkerSignature } from './agent-runtime.mjs';
import { terminalArguments } from './terminal-sessions.mjs';

const KEYS = new Set(['id','providerKind','backend','model','endpoint','credentialEnv',
  'credentialVersion','contextWindow','maxOutputTokens','usageProtocol']);
const text = (value, name) => {
  if (typeof value !== 'string' || !value.trim() || /[\r\n\0]/.test(value)) {
    throw new TypeError(`Invalid local profile ${name}`);
  }
  return value;
};

export function normalizeLocalAgentProfile(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value) ||
      Object.keys(value).some(key => !KEYS.has(key))) throw new TypeError('Invalid local profile fields');
  if (value.providerKind !== 'local' || value.backend !== 'claude') throw new TypeError('Only explicit local Claude runner profiles are supported');
  const id=text(value.id,'id');
  if (!/^[a-z0-9][a-z0-9-]{0,63}$/.test(id)) throw new TypeError('Invalid local profile ID');
  const model=text(value.model,'model');
  const url=new URL(text(value.endpoint,'endpoint'));
  if (url.protocol!=='http:' || !['127.0.0.1','[::1]'].includes(url.hostname) ||
      !url.port || url.username || url.password || url.search || url.hash || url.pathname!=='/') {
    throw new TypeError('Local endpoint must be a credential-free loopback bridge URL');
  }
  const credentialEnv=text(value.credentialEnv,'credentialEnv');
  if (!/^OFFICESTRA_LOCAL_[A-Z0-9_]+_TOKEN$/.test(credentialEnv)) throw new TypeError('Use a dedicated local credential environment reference');
  const credentialVersion=text(value.credentialVersion,'credentialVersion');
  if (![32768,65536].includes(value.contextWindow) || value.maxOutputTokens!==4096) throw new TypeError('Only measured 32K/64K contexts with 4096 output are enabled');
  if (value.usageProtocol!=='anthropic-normalized-v1') throw new TypeError('A usage-normalizing compatibility bridge is required');
  return Object.freeze({id,providerKind:'local',backend:'claude',model,
    endpoint:url.origin,credentialEnv,credentialVersion,contextWindow:value.contextWindow,
    maxOutputTokens:4096,usageProtocol:value.usageProtocol});
}

function localOptions(args, mode) {
  const result=[];
  for (let i=0;i<args.length;i++) {
    // User prompt and identity are data, even if they look like an option.
    if ((args[i]==='-p' && mode==='gui') ||
        ['--settings','--permission-mode','--model','--append-system-prompt','--resume',
          '--output-format','--input-format'].includes(args[i])) {
      result.push(args[i],args[++i]);continue;
    }
    // These flags belong to the cloud model. Gateway effort delivery has not
    // been verified, so do not offer a setting that is silently ignored.
    if (args[i]==='--effort') {i++;continue;}
    if (args[i]==='--prompt-suggestions') {result.push(args[i],'false');i++;continue;}
    result.push(args[i]);
  }
  return [...result,'--setting-sources',''];
}

export function createLocalAgentLaunch({profile,character,mode='gui',prompt='',
  previousSessionID=null,workdir,baseEnvironment=process.env,hookPath,nodePath,
  executable=locateExecutable(character)}) {
  const p=normalizeLocalAgentProfile(profile);
  if (!character || character.backend!=='claude' || character.model!==p.model) throw new TypeError('Character runner/model must match the local profile');
  if (character.fastMode===true) throw new TypeError('Local Fast mode is not verified');
  if (character.effort!=='default') throw new TypeError('Local profile requires default effort; configurable effort is not verified');
  if (!['plan','auto','acceptEdits','bypassPermissions'].includes(character.permission)) throw new TypeError('Invalid Claude permission');
  if (typeof workdir!=='string' || !workdir.startsWith('/') || /[\r\n\0]/.test(workdir)) throw new TypeError('Absolute workdir required');
  if (!['gui','terminal','persistent'].includes(mode)) throw new TypeError('Invalid local execution mode');
  if (previousSessionID!==null && (typeof previousSessionID!=='string' || !previousSessionID.trim() || /[\r\n\0]/.test(previousSessionID))) throw new TypeError('Invalid resume session');
  const credential=baseEnvironment[p.credentialEnv];
  if (typeof credential!=='string' || !credential || /[\r\n\0]/.test(credential)) throw new TypeError('Local bridge credential is unavailable');
  const clean=Object.fromEntries(Object.entries(baseEnvironment).filter(([key]) =>
    !/^(?:ANTHROPIC_|CLAUDE_|OPENAI_|OFFICESTRA_LOCAL_|DISABLE_AUTO_COMPACT$|DISABLE_COMPACT$|MAX_THINKING_TOKENS$)/.test(key)));
  const env=executionEnvironment(character,clean,{workdir});
  delete env.CLAUDE_AUTOCOMPACT_PCT_OVERRIDE;
  Object.assign(env,{
    ANTHROPIC_BASE_URL:p.endpoint,ANTHROPIC_AUTH_TOKEN:credential,ANTHROPIC_API_KEY:credential,
    CLAUDE_CODE_USE_GATEWAY:'1',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:'1',
    CLAUDE_CODE_MAX_RETRIES:'0',CLAUDE_CODE_MAX_CONTEXT_TOKENS:String(p.contextWindow),
    CLAUDE_CODE_MAX_OUTPUT_TOKENS:String(p.maxOutputTokens),MAX_THINKING_TOKENS:'0',
    ANTHROPIC_DEFAULT_HAIKU_MODEL:p.model,ANTHROPIC_DEFAULT_SONNET_MODEL:p.model,
    ANTHROPIC_DEFAULT_OPUS_MODEL:p.model,DISABLE_TELEMETRY:'1',
  });
  const args=mode==='terminal'
    ? terminalArguments({character,previousSessionID,workdir,hookPath,nodePath})
    : mode==='persistent' ? claudePersistentArguments(character,previousSessionID)
    : buildArguments({character,prompt,previousSessionID,workdir});
  const normalizedArgs=localOptions(args,mode);
  // Secret-free worker identity; callers must rotate credentialVersion with a
  // credential change. Do not put credential values into logs/signatures.
  const workerIdentity=claudePersistentWorkerSignature({character,workdir,executable});
  const signature=createHash('sha256').update(JSON.stringify({profile:p,mode,workerIdentity})).digest('hex');
  return {executable,backend:'claude',providerKind:'local',profile:p,args:normalizedArgs,env,cwd:workdir,signature};
}
