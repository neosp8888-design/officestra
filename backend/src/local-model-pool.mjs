import { createHash } from 'node:crypto';
import { llamaServerArguments, LLAMA_SERVER_SHA256 } from './local-llama-host.mjs';
import { localHostLoadConfig, localHostMemoryExceeded } from './local-provider-host.mjs';

// Protocol, credentials and reasoning belong to each front door. Only the
// verified host identity and actual model load configuration identify a GPU job.
export function localModelPoolKey({host,profile}) {
  const identity=Object.fromEntries(Object.keys(host).sort().map(k=>[k,host[k]]));
  const runtime=profile.runtime??'lm-studio';
  const load=profile.runtime==='llama-cpp-b10982'
    ? [LLAMA_SERVER_SHA256,llamaServerArguments(profile)] : localHostLoadConfig(profile);
  return createHash('sha256').update(JSON.stringify([identity,runtime,profile.model,load])).digest('hex');
}

// One FIFO per loaded model. A confirmed cancellation may keep a model warm;
// failures or unconfirmed cancellation invalidate it before another may run.
export class LocalModelPool {
  constructor({idleMs=180000,warmPollMs=3000,retainModels=false}={}){this.groups=new Map();this.idleMs=idleMs;this.warmPollMs=warmPollMs;this.retainModels=retainModels;}
  group(definition){
    const key=localModelPoolKey(definition);
    if(!this.groups.has(key))this.groups.set(key,{key,profileIds:new Set(),hostKey:JSON.stringify([definition.host.address,definition.host.sshPort]),retain:this.retainModels&&definition.profile.runtime==='llama-cpp-b10982',locked:false,queue:[],record:null});
    const group=this.groups.get(key);group.profileIds.add(definition.profile.id);return group;
  }
  acquire(group,signal,onWait=()=>{}) {
    if(signal.aborted)return Promise.reject(new Error('Local request cancelled'));
    return new Promise((resolve,reject)=>{
      const waiter={resolve,reject,signal};
      waiter.abort=()=>{
        const index=group.queue.indexOf(waiter);
        if(index<0)return;
        group.queue.splice(index,1);
        reject(new Error('Local request cancelled'));
      };
      signal.addEventListener('abort',waiter.abort,{once:true});
      group.queue.push(waiter);
      if(group.locked)onWait();
      this.dispatch(group);
    }).then(async unlock=>{
      try{await group.record?.recovering;return unlock;}
      catch(error){await unlock().catch(()=>{});throw error;}
    });
  }
  dispatch(group){
    if(group.locked)return;
    const waiter=group.queue.shift();
    if(!waiter)return;
    waiter.signal.removeEventListener('abort',waiter.abort);
    group.locked=true;
    let released=false;
    waiter.resolve(async()=>{
      if(released)return;released=true;
      // Keep the lock through cleanup, including failed cleanup. The next
      // borrower must see the failed record and cannot start another server.
      try{await this.maybeRelease(group);}
      finally{group.locked=false;this.dispatch(group);}
    });
  }
  async dispose(group,record){
    this.clearWarm(record);
    record.valid=false;
    if(!record.releasing){
      record.releaseError=null;
      record.releasing=Promise.resolve().then(()=>record.resource.release());
    }
    const releasing=record.releasing;
    try{await releasing;}
    catch(error){
      // Keep the unconfirmed ownership record, but allow the next explicit
      // cleanup attempt to contact the host again after SSH recovers.
      if(record.releasing===releasing){record.releasing=null;record.releaseError=error;}
      throw error;
    }
    if(group.record===record)group.record=null;
  }
  async maybeRelease(group){
    const record=group.record;
    if(record&&!record.refs.size&&!group.queue.length&&!record.warm){
      // Unlocking the failed operation is not a new recovery request.
      if(record.releaseError)throw record.releaseError;
      if(group.retain&&record.valid)this.keepWarm(group,record);
      else await this.dispose(group,record);
    }
  }
  clearWarm(record){
    clearTimeout(record.warmTimer);clearInterval(record.warmMonitor);
    record.warm=false;record.warmTimer=null;record.warmMonitor=null;
  }
  keepWarm(group,record){
    this.clearWarm(record);
    record.warm=true;
    if(!group.retain){record.warmTimer=setTimeout(()=>{
      record.warm=false;
      record.warmTimer=null;
      if(!group.locked)this.maybeRelease(group).catch(()=>{});
    },this.idleMs);record.warmTimer.unref();}
    let sampling=false;
    record.warmMonitor=setInterval(async()=>{
      if(sampling||group.locked||record.refs.size)return;
      sampling=true;let unsafe=false;
      try{const sample=await record.resource.sample();unsafe=sample.busy||localHostMemoryExceeded(sample);}
      catch{unsafe=true;}
      finally{sampling=false;}
      if(unsafe&&record.warm&&group.record===record&&!group.locked&&!record.refs.size)await this.dispose(group,record).catch(()=>{});
    },this.warmPollMs);record.warmMonitor.unref();
  }
  async retire(group){
    if(group.record)this.clearWarm(group.record);
    if(!group.locked&&group.record&&!group.record.refs.size&&!group.queue.length)await this.dispose(group,group.record);
  }
  async shutdown(){
    await Promise.all([...this.groups.values()].map(async group=>{
      await group.record?.recovering;
      if(group.record)await this.dispose(group,group.record);
    }));
  }
  async borrow(group,start){
    if(!group.locked)throw new Error('Model request lease required');
    if(group.record&&(!group.record.valid||!group.record.resource.alive()))await this.dispose(group,group.record);
    if(!group.record){
      // A different model may use the GPU as soon as the current request ends.
      // Invalidate idle bridges, preserving their stable CLI front doors.
      for(const peer of this.groups.values())if(peer!==group&&peer.hostKey===group.hostKey&&!peer.locked&&peer.record)await this.dispose(peer,peer.record);
      const resource=await start();
      group.record={resource,refs:new Set(),valid:true};
    }
    const record=group.record,ref={};record.refs.add(ref);
    this.clearWarm(record);
    let releaseTask;
    return {
      ...record.resource,
      alive:()=>record.valid&&record.resource.alive(),
      release:(reason='manual')=>releaseTask??=(async()=>{
        record.refs.delete(ref);
        if(reason==='Request cancelled'&&record.valid&&record.resource.waitForIdle){
          record.recovering=(async()=>{
            try{await record.resource.waitForIdle();}
            catch{await this.dispose(group,record);return;}
            if(record.valid)this.keepWarm(group,record);
          })();
          try{await record.recovering;}finally{record.recovering=null;}
          return;
        }
        if(!['manual','idle'].includes(reason))await this.dispose(group,record);
        else if(!group.locked)await this.maybeRelease(group);
      })(),
    };
  }
}
