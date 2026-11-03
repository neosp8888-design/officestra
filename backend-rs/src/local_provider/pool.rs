// 4090 모델 풀. Node local-model-pool.mjs를 옮겼다. 적재한 모델마다 FIFO 하나를 두고, 확인된 취소만 모델을 데워 둔다.
// 실패하거나 확인되지 않은 취소는 다음 요청 전에 모델을 내린다. 상태는 std Mutex 하나에 두고 await 동안 쥐지 않는다.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt, Shared, join_all};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use super::host::{HostResource, ResourceSample, local_host_load_config, local_host_memory_exceeded, sorted_host_identity};
use super::llama::{LLAMA_SERVER_SHA256, llama_server_arguments};
use super::{AbortSignal, LocalDefinition, LocalError};
use crate::js::to_js_json;
use crate::local_model_capabilities::DIRECT_RUNTIME;

pub type SharedResult = Shared<BoxFuture<'static, Result<(), LocalError>>>;

/// Node async 함수 호출처럼 바로 돌기 시작하고, 기다리는 쪽이 사라져도 끝까지 돈다.
pub(crate) fn spawn_shared(future: impl Future<Output = Result<(), LocalError>> + Send + 'static) -> SharedResult {
    let handle = tokio::spawn(future);
    async move { handle.await.unwrap_or_else(|error| Err(LocalError::plain(error.to_string()))) }.boxed().shared()
}

fn cancelled() -> LocalError {
    LocalError::plain("Local request cancelled")
}

/// localModelPoolKey. 프로토콜·자격 증명·추론은 앞문마다 다르다. 검증한 호스트 정체와 실제 적재 설정만 GPU 작업을 가른다.
pub fn local_model_pool_key(definition: &LocalDefinition) -> Result<String, String> {
    let profile = &definition.profile;
    let runtime = profile.runtime.clone().unwrap_or_else(|| "lm-studio".into());
    let load = if profile.runtime.as_deref() == Some(DIRECT_RUNTIME) {
        json!([LLAMA_SERVER_SHA256, llama_server_arguments(profile)?])
    } else {
        local_host_load_config(profile)?
    };
    let bytes = to_js_json(&json!([sorted_host_identity(&definition.host), runtime, profile.model, load])).map_err(|error| error.to_string())?;
    Ok(Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) struct Record {
    pub(crate) id: u64,
    pub(crate) resource: Arc<dyn HostResource>,
    refs: HashSet<u64>,
    pub(crate) valid: bool,
    warm: bool,
    warm_stop: Option<AbortSignal>,
    releasing: Option<SharedResult>,
    release_error: Option<LocalError>,
    recovering: Option<SharedResult>,
}

struct Waiter {
    id: u64,
    sender: oneshot::Sender<Unlock>,
}

pub(crate) struct Group {
    pub(crate) key: String,
    pub(crate) profile_ids: Vec<String>,
    host_key: String,
    pub(crate) retain: bool,
    pub(crate) locked: bool,
    queue: VecDeque<Waiter>,
    pub(crate) record: Option<Record>,
    /// 아래는 Node 서비스가 그룹 객체에 붙이던 모델 실행·중지 상태다.
    pub(crate) control_state: Option<&'static str>,
    pub(crate) control_error: Option<String>,
    pub(crate) control_abort: Option<AbortSignal>,
    pub(crate) paused: bool,
}

impl Group {
    /// `group.record?.valid && group.record.resource.alive()`.
    pub(crate) fn loaded(&self) -> bool {
        self.record.as_ref().is_some_and(|record| record.valid && record.resource.alive())
    }

    fn current(&mut self, id: u64) -> Option<&mut Record> {
        self.record.as_mut().filter(|record| record.id == id)
    }
}

#[derive(Default)]
pub(crate) struct PoolState {
    pub(crate) groups: Vec<Group>,
    next_id: u64,
}

impl PoolState {
    pub(crate) fn group(&self, key: &str) -> Option<&Group> {
        self.groups.iter().find(|group| group.key == key)
    }

    pub(crate) fn group_mut(&mut self, key: &str) -> Option<&mut Group> {
        self.groups.iter_mut().find(|group| group.key == key)
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

pub(crate) struct PoolInner {
    pub(crate) state: Mutex<PoolState>,
    idle: Duration,
    warm_poll: Duration,
    retain_models: bool,
}

#[derive(Clone)]
pub struct ModelPool(pub(crate) Arc<PoolInner>);

/// acquire()가 넘기는 잠금 해제. 부르지 않고 버려지면(요청 future 취소) 대신 풀어 다음 요청이 멈추지 않게 한다.
pub struct Unlock {
    pool: Arc<PoolInner>,
    key: String,
    released: bool,
}

impl Unlock {
    /// 정리 실패도 포함해 정리 동안 잠금을 쥔다. 다음 빌리는 쪽은 실패 기록을 보고 다른 서버를 띄우지 않는다.
    pub async fn release(mut self) -> Result<(), LocalError> {
        self.released = true;
        release_group(Arc::clone(&self.pool), self.key.clone()).await
    }
}

impl Drop for Unlock {
    fn drop(&mut self) {
        if !self.released
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(release_group(Arc::clone(&self.pool), self.key.clone()));
        }
    }
}

async fn release_group(pool: Arc<PoolInner>, key: String) -> Result<(), LocalError> {
    let result = maybe_release(&pool, &key).await;
    if let Some(group) = pool.state.lock().unwrap().group_mut(&key) {
        group.locked = false;
    }
    dispatch(&pool, &key);
    result
}

fn dispatch(pool: &Arc<PoolInner>, key: &str) {
    let mut state = pool.state.lock().unwrap();
    let Some(group) = state.group_mut(key) else {
        return;
    };
    if group.locked {
        return;
    }
    let Some(waiter) = group.queue.pop_front() else {
        return;
    };
    group.locked = true;
    // 받는 쪽이 이미 사라졌으면 돌아온 Unlock이 버려지며 풀린다.
    let _ = waiter.sender.send(Unlock { pool: Arc::clone(pool), key: key.to_string(), released: false });
}

fn clear_warm(record: &mut Record) {
    if let Some(stop) = record.warm_stop.take() {
        stop.abort();
    }
    record.warm = false;
}

/// dispose. 현재 기록이 아니면 이미 정리가 끝난 기록이라 할 일이 없다.
async fn dispose(pool: &Arc<PoolInner>, key: &str, id: u64) -> Result<(), LocalError> {
    let releasing = {
        let mut state = pool.state.lock().unwrap();
        let Some(record) = state.group_mut(key).and_then(|group| group.current(id)) else {
            return Ok(());
        };
        clear_warm(record);
        record.valid = false;
        if record.releasing.is_none() {
            record.release_error = None;
            let resource = Arc::clone(&record.resource);
            record.releasing = Some(spawn_shared(async move { resource.release().await }));
        }
        record.releasing.clone().unwrap()
    };
    let result = releasing.clone().await;
    let mut state = pool.state.lock().unwrap();
    let Some(group) = state.group_mut(key) else {
        return result;
    };
    match result {
        Err(error) => {
            // 확인 못 한 소유 기록은 남기되, SSH가 돌아온 뒤 다음 정리 요청이 다시 연락할 수 있게 한다.
            if let Some(record) = group.current(id)
                && record.releasing.as_ref().is_some_and(|current| current.ptr_eq(&releasing))
            {
                record.releasing = None;
                record.release_error = Some(error.clone());
            }
            Err(error)
        }
        Ok(()) => {
            if group.current(id).is_some() {
                group.record = None;
            }
            Ok(())
        }
    }
}

async fn maybe_release(pool: &Arc<PoolInner>, key: &str) -> Result<(), LocalError> {
    let action = {
        let state = pool.state.lock().unwrap();
        let Some(group) = state.group(key) else {
            return Ok(());
        };
        match &group.record {
            Some(record) if record.refs.is_empty() && group.queue.is_empty() && !record.warm => {
                // 실패한 작업의 잠금 해제는 새 복구 요청이 아니다.
                if let Some(error) = &record.release_error {
                    return Err(error.clone());
                }
                Some((record.id, group.retain && record.valid))
            }
            _ => None,
        }
    };
    match action {
        Some((id, true)) => {
            keep_warm(pool, key, id);
            Ok(())
        }
        Some((id, false)) => dispose(pool, key, id).await,
        None => Ok(()),
    }
}

/// keepWarm. 상주 모델이 아니면 idleMs 뒤 내리고, 모두 ComfyUI 사용·메모리 초과·측정 실패면 바로 내린다.
fn keep_warm(pool: &Arc<PoolInner>, key: &str, id: u64) {
    let mut state = pool.state.lock().unwrap();
    let Some(group) = state.group_mut(key) else {
        return;
    };
    let retain = group.retain;
    let Some(record) = group.current(id) else {
        return;
    };
    clear_warm(record);
    record.warm = true;
    let stop = AbortSignal::new();
    record.warm_stop = Some(stop.clone());
    let resource = Arc::clone(&record.resource);
    if !retain {
        let (pool, key, stop) = (Arc::clone(pool), key.to_string(), stop.clone());
        let idle = pool.idle;
        tokio::spawn(async move {
            tokio::select! {
                () = stop.aborted() => return,
                () = tokio::time::sleep(idle) => {}
            }
            let locked = {
                let mut state = pool.state.lock().unwrap();
                let Some(group) = state.group_mut(&key) else {
                    return;
                };
                let locked = group.locked;
                if let Some(record) = group.current(id) {
                    record.warm = false;
                }
                locked
            };
            if !locked {
                let _ = maybe_release(&pool, &key).await;
            }
        });
    }
    let (pool, key) = (Arc::clone(pool), key.to_string());
    tokio::spawn(async move {
        let period = pool.warm_poll;
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = stop.aborted() => return,
                _ = ticks.tick() => {}
            }
            let idle = |pool: &PoolInner| {
                let mut state = pool.state.lock().unwrap();
                state.group_mut(&key).is_some_and(|group| !group.locked && group.current(id).is_some_and(|record| record.refs.is_empty()))
            };
            if !idle(&pool) {
                continue;
            }
            let unsafe_sample = match resource.sample().await {
                Ok(sample) => sample.busy() || local_host_memory_exceeded(&sample),
                Err(_) => true,
            };
            let warm = {
                let mut state = pool.state.lock().unwrap();
                state.group_mut(&key).and_then(|group| group.current(id)).is_some_and(|record| record.warm)
            };
            if unsafe_sample && warm && idle(&pool) {
                let _ = dispose(&pool, &key, id).await;
            }
        }
    });
}

/// borrow()가 돌려주는 빌린 자원. alive는 기록이 아직 유효할 때만 참이다.
pub struct Borrowed {
    pool: Arc<PoolInner>,
    key: String,
    record_id: u64,
    ref_id: u64,
    resource: Arc<dyn HostResource>,
    release_task: Mutex<Option<SharedResult>>,
}

impl Borrowed {
    pub fn upstream(&self) -> String {
        self.resource.upstream()
    }

    pub fn sample(&self) -> BoxFuture<'static, Result<ResourceSample, LocalError>> {
        self.resource.sample()
    }

    pub fn alive(&self) -> bool {
        let valid = {
            let mut state = self.pool.state.lock().unwrap();
            state.group_mut(&self.key).and_then(|group| group.current(self.record_id)).is_some_and(|record| record.valid)
        };
        valid && self.resource.alive()
    }

    pub fn reconnect_address(&self, address: &str) -> Result<(), LocalError> {
        self.resource.reconnect_address(address)
    }

    /// release(reason). 처음 부른 이유로 한 번만 돈다.
    pub fn release(&self, reason: &str) -> SharedResult {
        let mut task = self.release_task.lock().unwrap();
        if let Some(task) = task.as_ref() {
            return task.clone();
        }
        let (pool, key, id, ref_id, resource, reason) =
            (Arc::clone(&self.pool), self.key.clone(), self.record_id, self.ref_id, Arc::clone(&self.resource), reason.to_string());
        let shared = spawn_shared(async move {
            let recover = {
                let mut state = pool.state.lock().unwrap();
                let record = state.group_mut(&key).and_then(|group| group.current(id));
                let valid = record.map(|record| {
                    record.refs.remove(&ref_id);
                    record.valid
                });
                reason == "Request cancelled" && valid == Some(true) && resource.supports_wait_for_idle()
            };
            if recover {
                let (recover_pool, recover_key) = (Arc::clone(&pool), key.clone());
                let recovering = spawn_shared(async move {
                    if resource.wait_for_idle().await.is_err() {
                        return dispose(&recover_pool, &recover_key, id).await;
                    }
                    let valid = {
                        let mut state = recover_pool.state.lock().unwrap();
                        state.group_mut(&recover_key).and_then(|group| group.current(id)).is_some_and(|record| record.valid)
                    };
                    if valid {
                        keep_warm(&recover_pool, &recover_key, id);
                    }
                    Ok(())
                });
                if let Some(record) = pool.state.lock().unwrap().group_mut(&key).and_then(|group| group.current(id)) {
                    record.recovering = Some(recovering.clone());
                }
                let result = recovering.await;
                if let Some(record) = pool.state.lock().unwrap().group_mut(&key).and_then(|group| group.current(id)) {
                    record.recovering = None;
                }
                return result;
            }
            if reason != "manual" && reason != "idle" {
                return dispose(&pool, &key, id).await;
            }
            let locked = pool.state.lock().unwrap().group(&key).is_some_and(|group| group.locked);
            if locked { Ok(()) } else { maybe_release(&pool, &key).await }
        });
        *task = Some(shared.clone());
        shared
    }
}

impl ModelPool {
    pub fn new(idle: Duration, warm_poll: Duration, retain_models: bool) -> Self {
        Self(Arc::new(PoolInner { state: Mutex::new(PoolState::default()), idle, warm_poll, retain_models }))
    }

    /// group(definition). 그룹 열쇠를 돌려준다.
    pub fn group(&self, definition: &LocalDefinition) -> Result<String, String> {
        let key = local_model_pool_key(definition)?;
        let mut state = self.0.state.lock().unwrap();
        if state.group(&key).is_none() {
            let host = definition.host.fields();
            let host_key = String::from_utf8(to_js_json(&json!([host.get("address"), host.get("sshPort")])).map_err(|error| error.to_string())?).unwrap_or_default();
            state.groups.push(Group {
                key: key.clone(),
                profile_ids: Vec::new(),
                host_key,
                retain: self.0.retain_models && definition.profile.runtime.as_deref() == Some(DIRECT_RUNTIME),
                locked: false,
                queue: VecDeque::new(),
                record: None,
                control_state: None,
                control_error: None,
                control_abort: None,
                paused: false,
            });
        }
        let group = state.group_mut(&key).unwrap();
        if !group.profile_ids.contains(&definition.profile.id) {
            group.profile_ids.push(definition.profile.id.clone());
        }
        Ok(key)
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.0.state.lock().unwrap()
    }

    /// acquire. 앞 요청이 잠갔으면 on_wait를 부르고 차례를 기다린다. 취소된 대기자는 줄에서 빠지고 재생되지 않는다.
    pub async fn acquire(&self, key: &str, signal: &AbortSignal, on_wait: impl FnOnce()) -> Result<Unlock, LocalError> {
        if signal.is_aborted() {
            return Err(cancelled());
        }
        let (sender, mut receiver) = oneshot::channel();
        let (id, locked) = {
            let mut state = self.0.state.lock().unwrap();
            let id = state.next();
            let Some(group) = state.group_mut(key) else {
                return Err(LocalError::plain("Unknown local model group"));
            };
            group.queue.push_back(Waiter { id, sender });
            (id, group.locked)
        };
        if locked {
            on_wait();
        }
        dispatch(&self.0, key);
        let outcome = tokio::select! {
            unlock = &mut receiver => Some(unlock),
            () = signal.aborted() => None,
        };
        let unlock = match outcome {
            Some(unlock) => unlock.map_err(|_| cancelled())?,
            None => {
                let removed = {
                    let mut state = self.0.state.lock().unwrap();
                    state.group_mut(key).and_then(|group| group.queue.iter().position(|waiter| waiter.id == id).map(|index| group.queue.remove(index))).is_some()
                };
                if removed {
                    return Err(cancelled());
                }
                receiver.await.map_err(|_| cancelled())?
            }
        };
        let recovering = self.0.state.lock().unwrap().group(key).and_then(|group| group.record.as_ref()).and_then(|record| record.recovering.clone());
        if let Some(recovering) = recovering
            && let Err(error) = recovering.await
        {
            let _ = unlock.release().await;
            return Err(error);
        }
        Ok(unlock)
    }

    /// borrow. 잠금을 쥔 요청만 부른다. 같은 호스트의 다른 모델은 쉬고 있을 때만 내린다(CLI 앞문은 그대로 둔다).
    pub async fn borrow(&self, key: &str, start: impl FnOnce() -> BoxFuture<'static, Result<Arc<dyn HostResource>, LocalError>>) -> Result<Borrowed, LocalError> {
        let current = {
            let state = self.0.state.lock().unwrap();
            let Some(group) = state.group(key).filter(|group| group.locked) else {
                return Err(LocalError::plain("Model request lease required"));
            };
            group.record.as_ref().map(|record| (record.id, record.valid && record.resource.alive()))
        };
        if let Some((id, false)) = current {
            dispose(&self.0, key, id).await?;
        }
        let missing = self.0.state.lock().unwrap().group(key).is_some_and(|group| group.record.is_none());
        if missing {
            let peers: Vec<(String, u64)> = {
                let state = self.0.state.lock().unwrap();
                let host_key = state.group(key).map(|group| group.host_key.clone()).unwrap_or_default();
                state
                    .groups
                    .iter()
                    .filter(|peer| peer.key != key && peer.host_key == host_key && !peer.locked)
                    .filter_map(|peer| peer.record.as_ref().map(|record| (peer.key.clone(), record.id)))
                    .collect()
            };
            for (peer, id) in peers {
                let still_idle = self.0.state.lock().unwrap().group(&peer).is_some_and(|group| !group.locked);
                if still_idle {
                    dispose(&self.0, &peer, id).await?;
                }
            }
            let resource = start().await?;
            let mut state = self.0.state.lock().unwrap();
            let id = state.next();
            if let Some(group) = state.group_mut(key) {
                group.record = Some(Record {
                    id,
                    resource,
                    refs: HashSet::new(),
                    valid: true,
                    warm: false,
                    warm_stop: None,
                    releasing: None,
                    release_error: None,
                    recovering: None,
                });
            }
        }
        let mut state = self.0.state.lock().unwrap();
        let ref_id = state.next();
        let Some(record) = state.group_mut(key).and_then(|group| group.record.as_mut()) else {
            return Err(LocalError::plain("Model request lease required"));
        };
        record.refs.insert(ref_id);
        clear_warm(record);
        Ok(Borrowed {
            pool: Arc::clone(&self.0),
            key: key.to_string(),
            record_id: record.id,
            ref_id,
            resource: Arc::clone(&record.resource),
            release_task: Mutex::new(None),
        })
    }

    /// `if(group.record) await dispose(group, group.record)`.
    pub async fn dispose_current(&self, key: &str) -> Result<(), LocalError> {
        let id = self.0.state.lock().unwrap().group(key).and_then(|group| group.record.as_ref()).map(|record| record.id);
        match id {
            Some(id) => dispose(&self.0, key, id).await,
            None => Ok(()),
        }
    }

    /// `if(group.record) keepWarm(group, group.record)`.
    pub fn keep_warm_current(&self, key: &str) {
        let id = self.0.state.lock().unwrap().group(key).and_then(|group| group.record.as_ref()).map(|record| record.id);
        if let Some(id) = id {
            keep_warm(&self.0, key, id);
        }
    }

    /// retire. 쉬는 기록만 내린다.
    pub async fn retire(&self, key: &str) -> Result<(), LocalError> {
        let idle = {
            let mut state = self.0.state.lock().unwrap();
            let Some(group) = state.group_mut(key) else {
                return Ok(());
            };
            let (locked, queued) = (group.locked, !group.queue.is_empty());
            group.record.as_mut().and_then(|record| {
                clear_warm(record);
                (!locked && record.refs.is_empty() && !queued).then_some(record.id)
            })
        };
        match idle {
            Some(id) => dispose(&self.0, key, id).await,
            None => Ok(()),
        }
    }

    /// shutdown. 회복 중인 기록을 기다린 뒤 모두 내린다. 첫 실패를 알린다.
    pub async fn shutdown(&self) -> Result<(), LocalError> {
        let keys: Vec<String> = self.0.state.lock().unwrap().groups.iter().map(|group| group.key.clone()).collect();
        let results = join_all(keys.into_iter().map(|key| {
            let pool = Arc::clone(&self.0);
            async move {
                let recovering = pool.state.lock().unwrap().group(&key).and_then(|group| group.record.as_ref()).and_then(|record| record.recovering.clone());
                if let Some(recovering) = recovering {
                    recovering.await?;
                }
                let id = pool.state.lock().unwrap().group(&key).and_then(|group| group.record.as_ref()).map(|record| record.id);
                match id {
                    Some(id) => dispose(&pool, &key, id).await,
                    None => Ok(()),
                }
            }
        }))
        .await;
        results.into_iter().collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use serde_json::Value;

    use super::super::normalize_local_definition;
    use super::*;
    use crate::local_model_capabilities::{MEROMERO_HOST_KEY, MEROMERO_MODEL_ID};

    pub(crate) fn definition_value() -> Value {
        json!({
            "profile": { "id": "shared-claude", "providerKind": "local", "backend": "claude", "model": MEROMERO_MODEL_ID, "runtime": DIRECT_RUNTIME,
                "kvCacheQuantization": "q8_0", "reasoning": "off", "endpoint": "http://127.0.0.1:41235", "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN",
                "credentialVersion": "v1", "contextWindow": 65536, "maxOutputTokens": 4096, "usageProtocol": "anthropic-normalized-v1" },
            "host": { "address": "127.0.0.1", "user": "test", "sshPort": 2222, "keyPath": "/tmp/test-key", "hostKeyAlias": "test-host", "modelKey": MEROMERO_HOST_KEY, "comfyPort": 8188 },
        })
    }

    fn definition() -> LocalDefinition {
        normalize_local_definition(&definition_value()).unwrap()
    }

    fn changed(change: impl FnOnce(&mut Value)) -> LocalDefinition {
        let mut value = definition_value();
        change(&mut value);
        normalize_local_definition(&value).unwrap()
    }

    /// 시험용 자원. release가 실패하게 하거나 alive·측정값을 바꿀 수 있다.
    pub(crate) struct FakeResource {
        pub(crate) alive: Arc<AtomicBool>,
        pub(crate) releases: Arc<AtomicUsize>,
        pub(crate) fail_release: Option<&'static str>,
        pub(crate) idle_check: bool,
        pub(crate) sample: ResourceSample,
    }

    impl HostResource for FakeResource {
        fn upstream(&self) -> String {
            "http://127.0.0.1:9".into()
        }

        fn sample(&self) -> BoxFuture<'static, Result<ResourceSample, LocalError>> {
            let sample = self.sample.clone();
            Box::pin(async move { Ok(sample) })
        }

        fn alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }

        fn release(&self) -> BoxFuture<'static, Result<(), LocalError>> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            let failure = self.fail_release;
            Box::pin(async move { failure.map_or(Ok(()), |message| Err(LocalError::plain(message))) })
        }

        fn supports_wait_for_idle(&self) -> bool {
            self.idle_check
        }
    }

    fn fake(alive: &Arc<AtomicBool>, releases: &Arc<AtomicUsize>, fail: Option<&'static str>) -> Arc<dyn HostResource> {
        Arc::new(FakeResource { alive: Arc::clone(alive), releases: Arc::clone(releases), fail_release: fail, idle_check: false, sample: ResourceSample::of(95.0, 50.0, false, None) })
    }

    fn pool() -> ModelPool {
        ModelPool::new(Duration::from_millis(180_000), Duration::from_millis(3000), false)
    }

    #[test]
    fn shares_actual_load_configuration() {
        let other = |value: &mut Value| {
            let profile = &mut value["profile"];
            for (key, field) in [("id", json!("shared-codex")), ("backend", json!("codex")), ("usageProtocol", json!("openai-responses-v1")), ("reasoning", json!("on")),
                ("maxOutputTokens", json!(8192)), ("credentialEnv", json!("OFFICESTRA_LOCAL_OTHER_TOKEN")), ("credentialVersion", json!("v2")), ("endpoint", json!("http://127.0.0.1:40001"))] {
                profile[key] = field;
            }
        };
        let key = local_model_pool_key(&definition()).unwrap();
        assert_eq!(key, local_model_pool_key(&changed(other)).unwrap());
        assert_ne!(key, local_model_pool_key(&changed(|value| { other(value); value["profile"]["contextWindow"] = json!(32768); })).unwrap());
        for field in ["address", "keyPath", "hostKeyAlias", "user", "modelKey"] {
            let value = if field == "keyPath" { "/different" } else { "different" };
            assert_ne!(key, local_model_pool_key(&changed(|v| { other(v); v["host"][field] = json!(value); })).unwrap(), "{field}");
        }
        let qwen = |value: &mut Value| {
            other(value);
            value["host"]["modelKey"] = json!("qwen3.8-27b");
            value["profile"]["model"] = json!("officestra-qwen38-27b-uncensored-q4km");
            value["profile"]["reasoning"] = json!("medium");
        };
        let qwen_key = local_model_pool_key(&changed(qwen)).unwrap();
        assert_ne!(key, qwen_key);
        let lmstudio = changed(|value| {
            qwen(value);
            value["profile"].as_object_mut().unwrap().shift_remove("runtime");
            value["profile"].as_object_mut().unwrap().shift_remove("reasoning");
        });
        assert_ne!(qwen_key, local_model_pool_key(&lmstudio).unwrap());
    }

    #[tokio::test]
    async fn failed_cleanup_blocks_replacement() {
        let pool = pool();
        let key = pool.group(&definition()).unwrap();
        let signal = AbortSignal::new();
        let (alive, releases) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicUsize::new(0)));
        let starts = Arc::new(AtomicUsize::new(0));
        let unlock = pool.acquire(&key, &signal, || {}).await.unwrap();
        let resource = {
            let (alive, releases, starts) = (alive.clone(), releases.clone(), starts.clone());
            pool.borrow(&key, move || Box::pin(async move { starts.fetch_add(1, Ordering::SeqCst); Ok(fake(&alive, &releases, Some("cleanup failed"))) })).await.unwrap()
        };
        assert_eq!(resource.release("Request cancelled").await.unwrap_err().message, "cleanup failed");
        assert_eq!(unlock.release().await.unwrap_err().message, "cleanup failed");
        let next = pool.acquire(&key, &signal, || {}).await.unwrap();
        let starts_again = starts.clone();
        let failed = pool.borrow(&key, move || Box::pin(async move { starts_again.fetch_add(1, Ordering::SeqCst); Err(LocalError::plain("unexpected start")) })).await;
        assert_eq!(failed.err().unwrap().message, "cleanup failed");
        assert_eq!(next.release().await.unwrap_err().message, "cleanup failed");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lost_tunnel_is_cleaned_before_new_borrow() {
        let pool = pool();
        let key = pool.group(&definition()).unwrap();
        let signal = AbortSignal::new();
        let (alive, releases) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicUsize::new(0)));
        let starts = Arc::new(AtomicUsize::new(0));
        let start = || {
            let (alive, releases, starts) = (alive.clone(), releases.clone(), starts.clone());
            move || -> BoxFuture<'static, Result<Arc<dyn HostResource>, LocalError>> {
                Box::pin(async move {
                    starts.fetch_add(1, Ordering::SeqCst);
                    alive.store(true, Ordering::SeqCst);
                    Ok(fake(&alive, &releases, None))
                })
            }
        };
        let unlock = pool.acquire(&key, &signal, || {}).await.unwrap();
        let old = pool.borrow(&key, start()).await.unwrap();
        unlock.release().await.unwrap();
        alive.store(false, Ordering::SeqCst);
        let unlock = pool.acquire(&key, &signal, || {}).await.unwrap();
        let fresh = pool.borrow(&key, start()).await.unwrap();
        assert!(!old.alive());
        assert!(fresh.alive());
        assert_eq!((starts.load(Ordering::SeqCst), releases.load(Ordering::SeqCst)), (2, 1));
        old.release("idle").await.unwrap();
        assert!(fresh.alive());
        fresh.release("manual").await.unwrap();
        unlock.release().await.unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 2);
    }

    struct GatedResource {
        attempts: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
        gate: Arc<tokio::sync::Notify>,
    }

    impl HostResource for GatedResource {
        fn upstream(&self) -> String {
            String::new()
        }
        fn sample(&self) -> BoxFuture<'static, Result<ResourceSample, LocalError>> {
            Box::pin(async { Ok(ResourceSample::of(1.0, 1.0, false, None)) })
        }
        fn alive(&self) -> bool {
            true
        }
        fn release(&self) -> BoxFuture<'static, Result<(), LocalError>> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let (fail, gate) = (self.fail.load(Ordering::SeqCst), Arc::clone(&self.gate));
            Box::pin(async move {
                if fail {
                    return Err(LocalError::plain("SSH failed"));
                }
                gate.notified().await;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn failed_cleanup_retries_once_for_concurrent_callers() {
        let pool = pool();
        let key = pool.group(&definition()).unwrap();
        let (attempts, fail, gate) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(true)), Arc::new(tokio::sync::Notify::new()));
        let resource: Arc<dyn HostResource> = Arc::new(GatedResource { attempts: attempts.clone(), fail: fail.clone(), gate: gate.clone() });
        let id = {
            let mut state = pool.state();
            let id = state.next();
            state.group_mut(&key).unwrap().record = Some(Record {
                id, resource, refs: HashSet::new(), valid: true, warm: false, warm_stop: None, releasing: None, release_error: None, recovering: None,
            });
            id
        };
        assert_eq!(dispose(&pool.0, &key, id).await.unwrap_err().message, "SSH failed");
        assert!(pool.state().group(&key).unwrap().record.as_ref().is_some_and(|record| !record.valid));
        fail.store(false, Ordering::SeqCst);
        let (a, b) = (tokio::spawn(dispose_owned(pool.clone(), key.clone(), id)), tokio::spawn(dispose_owned(pool.clone(), key.clone(), id)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "동시에 다시 정리해도 원격 정리는 한 번이다");
        assert!(pool.state().group(&key).unwrap().record.is_some(), "정리가 끝나기 전에는 소유 기록을 남긴다");
        gate.notify_one();
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
        assert!(pool.state().group(&key).unwrap().record.is_none());
    }

    async fn dispose_owned(pool: ModelPool, key: String, id: u64) -> Result<(), LocalError> {
        dispose(&pool.0, &key, id).await
    }

    #[tokio::test]
    async fn queue_is_fifo_and_drops_cancelled_waiters() {
        let pool = pool();
        let key = pool.group(&definition()).unwrap();
        let signal = AbortSignal::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let unlock = pool.acquire(&key, &signal, || {}).await.unwrap();
        let cancel = AbortSignal::new();
        let next = |number: u32, signal: AbortSignal| {
            let (pool, key, order) = (pool.clone(), key.clone(), order.clone());
            tokio::spawn(async move {
                let unlock = pool.acquire(&key, &signal, || {}).await?;
                order.lock().unwrap().push(number);
                unlock.release().await
            })
        };
        let a = next(1, signal.clone());
        tokio::time::sleep(Duration::from_millis(5)).await;
        let b = next(2, cancel.clone());
        tokio::time::sleep(Duration::from_millis(5)).await;
        let c = next(3, signal.clone());
        tokio::time::sleep(Duration::from_millis(5)).await;
        cancel.abort();
        unlock.release().await.unwrap();
        a.await.unwrap().unwrap();
        assert!(b.await.unwrap().is_err());
        c.await.unwrap().unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 3]);
    }

    #[tokio::test]
    async fn dropped_unlock_frees_the_queue() {
        let pool = pool();
        let key = pool.group(&definition()).unwrap();
        let signal = AbortSignal::new();
        drop(pool.acquire(&key, &signal, || {}).await.unwrap());
        let next = tokio::time::timeout(Duration::from_secs(1), pool.acquire(&key, &signal, || {})).await;
        assert!(next.unwrap().is_ok());
    }

    #[tokio::test]
    async fn warm_model_without_bridge_releases_on_reservation_or_memory_guard() {
        for sample in [ResourceSample::of(95.0, 50.0, true, None), ResourceSample::of(98.0, 50.0, false, None), ResourceSample::of(95.0, 90.0, false, None)] {
            let pool = ModelPool::new(Duration::from_millis(1000), Duration::from_millis(5), false);
            let key = pool.group(&definition()).unwrap();
            let signal = AbortSignal::new();
            let (alive, releases) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicUsize::new(0)));
            let unlock = pool.acquire(&key, &signal, || {}).await.unwrap();
            let resource = {
                let (alive, releases, sample) = (alive.clone(), releases.clone(), sample.clone());
                pool.borrow(&key, move || -> BoxFuture<'static, Result<Arc<dyn HostResource>, LocalError>> {
                    Box::pin(async move { Ok(Arc::new(FakeResource { alive, releases, fail_release: None, idle_check: true, sample }) as Arc<dyn HostResource>) })
                })
                .await
                .unwrap()
            };
            resource.release("Request cancelled").await.unwrap();
            unlock.release().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(releases.load(Ordering::SeqCst), 1);
            pool.shutdown().await.unwrap();
            assert_eq!(releases.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn different_model_evicts_only_idle_same_host_resources() {
        let pool = ModelPool::new(Duration::from_millis(180_000), Duration::from_millis(3000), true);
        let signal = AbortSignal::new();
        let a = pool.group(&definition()).unwrap();
        let b = pool.group(&changed(|value| value["profile"]["contextWindow"] = json!(32768))).unwrap();
        let (alive, releases) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicUsize::new(0)));
        let start = || {
            let (alive, releases) = (alive.clone(), releases.clone());
            move || -> BoxFuture<'static, Result<Arc<dyn HostResource>, LocalError>> { Box::pin(async move { Ok(fake(&alive, &releases, None)) }) }
        };
        let unlock_a = pool.acquire(&a, &signal, || {}).await.unwrap();
        let old = pool.borrow(&a, start()).await.unwrap();
        let unlock_b = pool.acquire(&b, &signal, || {}).await.unwrap();
        // 호스트 중재가 A 생성 중인 시도를 거절한다. 풀은 임대 중인 동료를 내리지 않는다.
        let busy = pool.borrow(&b, || Box::pin(async { Err(LocalError::plain("GPU busy")) })).await;
        assert_eq!(busy.err().unwrap().message, "GPU busy");
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        unlock_b.release().await.unwrap();
        unlock_a.release().await.unwrap();
        let next = pool.acquire(&b, &signal, || {}).await.unwrap();
        let fresh = pool.borrow(&b, start()).await.unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert!(!old.alive());
        assert!(fresh.alive());
        next.release().await.unwrap();
        pool.shutdown().await.unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 2);
    }
}
