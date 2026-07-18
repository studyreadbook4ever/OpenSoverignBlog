use std::{
    sync::{
        Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use redis::{
    IntoConnectionInfo,
    aio::{ConnectionManager, ConnectionManagerConfig},
    sentinel::{SentinelClient, SentinelNodeConnectionInfo, SentinelServerType},
};
use serde::Serialize;
use tokio::{
    sync::{Mutex, MutexGuard, RwLock},
    time::Instant,
};

use crate::config::{RedisSettings, RedisTopology};

const CACHE_SCHEMA: &str = "cache-v1";
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(100);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);
// A 64 MiB body expands to just under 86 MiB as signed JSON/Base64. Reject
// larger Redis values in Lua before GET transfers or allocates them in Rust.
const MAX_CACHE_VALUE_BYTES: usize = 86 * 1024 * 1024;
const LOOKUP_SCRIPT: &str = r#"
local generation = redis.call('GET', KEYS[1])
if not generation then
  redis.call('SET', KEYS[1], ARGV[1], 'NX')
  generation = redis.call('GET', KEYS[1])
end
local value_key = ARGV[2] .. generation .. ':' .. ARGV[3]
local value = false
if redis.call('STRLEN', value_key) <= tonumber(ARGV[4]) then
  value = redis.call('GET', value_key)
end
return {generation, value}
"#;
const STORE_SCRIPT: &str = r#"
local epoch = redis.call('GET', KEYS[1])
if epoch == ARGV[1] then
  redis.call('SET', ARGV[2] .. epoch .. ':' .. ARGV[3], ARGV[4], 'EX', ARGV[5])
  return 1
end
return 0
"#;

#[derive(Clone)]
pub struct SemanticCache {
    inner: Arc<Inner>,
}

struct Inner {
    settings: RedisSettings,
    manager: RwLock<ManagerSlot>,
    reconnect: ReconnectControl,
    generation_change: Mutex<()>,
    active_mutations: AtomicU64,
    // (invalidation sequence << 1) | dirty. Publishing a generation uses a
    // compare-exchange, so an older Redis reply cannot clear newer dirtiness.
    coherence: AtomicU64,
    state: AtomicU8,
    hits: AtomicU64,
    misses: AtomicU64,
    errors: AtomicU64,
    last_success_unix: AtomicU64,
    last_error: RwLock<Option<String>>,
}

#[derive(Default)]
struct ManagerSlot {
    manager: Option<ConnectionManager>,
    generation: u64,
}

struct ManagerLease {
    manager: ConnectionManager,
    generation: u64,
}

struct ReconnectControl {
    attempt: Mutex<()>,
    breaker: StdMutex<BreakerState>,
}

struct BreakerState {
    consecutive_failures: u32,
    retry_not_before: Option<Instant>,
}

impl ReconnectControl {
    fn new() -> Self {
        Self {
            attempt: Mutex::new(()),
            breaker: StdMutex::new(BreakerState {
                consecutive_failures: 0,
                retry_not_before: None,
            }),
        }
    }

    /// Only one request may establish a connection. Other requests fail fast and
    /// use the canonical origin while that attempt is in flight.
    fn begin_attempt(&self) -> Result<MutexGuard<'_, ()>> {
        self.ensure_retry_allowed()?;
        let guard = self
            .attempt
            .try_lock()
            .map_err(|_| anyhow!("Redis reconnect is already in progress"))?;
        // A previous attempt may have opened the breaker immediately before this
        // request acquired the gate.
        self.ensure_retry_allowed()?;
        Ok(guard)
    }

    fn ensure_retry_allowed(&self) -> Result<()> {
        let breaker = self.breaker();
        if breaker
            .retry_not_before
            .is_some_and(|deadline| deadline > Instant::now())
        {
            return Err(anyhow!(
                "Redis reconnect is temporarily paused after a recent failure"
            ));
        }
        Ok(())
    }

    fn record_failure(&self) {
        let mut breaker = self.breaker();
        breaker.consecutive_failures = breaker.consecutive_failures.saturating_add(1);
        let exponent = breaker.consecutive_failures.saturating_sub(1).min(31);
        let delay = RECONNECT_BACKOFF_MIN
            .saturating_mul(1_u32 << exponent)
            .min(RECONNECT_BACKOFF_MAX);
        breaker.retry_not_before = Some(Instant::now() + delay);
    }

    fn record_success(&self) {
        let mut breaker = self.breaker();
        breaker.consecutive_failures = 0;
        breaker.retry_not_before = None;
    }

    fn breaker(&self) -> StdMutexGuard<'_, BreakerState> {
        self.breaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug, Clone)]
pub struct CacheLookup {
    pub epoch: String,
    pub value: Option<Vec<u8>>,
}

/// Keeps derivative-cache reads suspended for the full lifetime of a
/// canonical mutation. Drop is synchronous so cancellation and error paths
/// cannot forget the post-operation dirty transition.
#[must_use = "hold this guard until the canonical mutation attempt has finished"]
pub struct CacheMutationGuard {
    cache: SemanticCache,
}

impl Drop for CacheMutationGuard {
    fn drop(&mut self) {
        self.cache.mark_dirty();
        self.cache
            .inner
            .active_mutations
            .fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    Active,
    Degraded,
    Connecting,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheSnapshot {
    pub provider: &'static str,
    pub role: &'static str,
    pub state: CacheState,
    pub required: bool,
    pub topology: &'static str,
    pub namespace: String,
    pub content_release: String,
    pub hits: u64,
    pub misses: u64,
    pub errors: u64,
    pub last_success_unix: Option<u64>,
    pub last_error: Option<String>,
}

impl SemanticCache {
    pub async fn connect(settings: RedisSettings) -> Result<Self> {
        let cache = Self {
            inner: Arc::new(Inner {
                settings,
                manager: RwLock::new(ManagerSlot::default()),
                reconnect: ReconnectControl::new(),
                generation_change: Mutex::new(()),
                active_mutations: AtomicU64::new(0),
                // A process start always rotates the Redis generation before it
                // may consume cache entries. This also closes the crash window
                // after a committed mutation whose invalidation lost its reply.
                coherence: AtomicU64::new(1),
                state: AtomicU8::new(0),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                errors: AtomicU64::new(0),
                last_success_unix: AtomicU64::new(0),
                last_error: RwLock::new(None),
            }),
        };
        let ready = async {
            cache.ping().await?;
            cache.repair_coherence().await?;
            Result::<()>::Ok(())
        }
        .await;
        if let Err(error) = ready {
            if cache.inner.settings.required {
                return Err(error).context(
                    "Redis is a required hot-path dependency; start the configured topology or set a reachable endpoint",
                );
            }
            tracing::warn!(%error, "Redis cache started degraded; public origin fallback remains available");
        }
        Ok(cache)
    }

    pub async fn ping(&self) -> Result<()> {
        self.ensure_no_active_mutations()?;
        if !self.reads_safe() {
            self.repair_coherence().await?;
        }
        let lease = self.manager().await?;
        let generation = lease.generation;
        let mut manager = lease.manager;
        let result: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut manager).await;
        match result {
            Ok(pong) if pong == "PONG" => {
                self.record_success().await;
                Ok(())
            }
            Ok(_) => {
                let error = anyhow!("Redis returned an unexpected PING response");
                self.handle_command_error(generation, &error).await;
                Err(error)
            }
            Err(source) => {
                let error = anyhow!(source).context("Redis PING failed");
                self.handle_command_error(generation, &error).await;
                Err(error)
            }
        }
    }

    pub async fn lookup(&self, route_hash: &str) -> Result<CacheLookup> {
        self.ensure_no_active_mutations()?;
        if !self.reads_safe() {
            self.repair_coherence().await?;
        }
        let lease = self.manager().await?;
        let generation = lease.generation;
        let mut manager = lease.manager;
        let result: redis::RedisResult<(String, Option<Vec<u8>>)> = redis::cmd("EVAL")
            .arg(LOOKUP_SCRIPT)
            .arg(1)
            .arg(self.epoch_key())
            .arg(fresh_generation())
            .arg(self.response_prefix())
            .arg(route_hash)
            .arg(MAX_CACHE_VALUE_BYTES)
            .query_async(&mut manager)
            .await;
        match result {
            Ok((epoch, value)) => {
                self.record_success().await;
                Ok(CacheLookup { epoch, value })
            }
            Err(error) => {
                let error = anyhow!(error).context("Redis cache lookup failed");
                self.handle_command_error(generation, &error).await;
                Err(error)
            }
        }
    }

    pub fn record_verified_hit(&self) {
        self.inner.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.inner.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Stores only when no mutation advanced the epoch while the origin was rendered.
    pub async fn store(&self, lookup_epoch: &str, route_hash: &str, value: &[u8]) -> Result<bool> {
        let lease = self.manager().await?;
        let generation = lease.generation;
        let mut manager = lease.manager;
        let result: redis::RedisResult<i64> = redis::cmd("EVAL")
            .arg(STORE_SCRIPT)
            .arg(1)
            .arg(self.epoch_key())
            .arg(lookup_epoch)
            .arg(self.response_prefix())
            .arg(route_hash)
            .arg(value)
            .arg(self.inner.settings.response_ttl_seconds)
            .query_async(&mut manager)
            .await;
        match result {
            Ok(stored) => {
                self.record_success().await;
                Ok(stored == 1)
            }
            Err(error) => {
                let error = anyhow!(error).context("Redis cache store failed");
                self.handle_command_error(generation, &error).await;
                Err(error)
            }
        }
    }

    /// Marks the cache dirty before a canonical mutation starts. This is
    /// synchronous so request cancellation after the SQLite commit cannot skip
    /// the safety transition.
    pub fn begin_mutation(&self) -> CacheMutationGuard {
        self.inner.active_mutations.fetch_add(1, Ordering::AcqRel);
        self.mark_dirty();
        CacheMutationGuard {
            cache: self.clone(),
        }
    }

    /// Rotates to a non-repeating generation after a mutation attempt. A plain counter is unsafe here:
    /// eviction of the epoch key could reset it and resurrect an older response.
    ///
    /// The dirty flag is set before attempting Redis. If the command fails, all
    /// subsequent cache reads must repair the generation or fall back to origin.
    pub async fn complete_mutation(&self) -> Result<()> {
        if self.active_mutations() != 0 {
            return Ok(());
        }
        let _change = self.inner.generation_change.lock().await;
        if self.active_mutations() != 0 {
            return Ok(());
        }
        self.rotate_until_safe().await
    }

    async fn repair_coherence(&self) -> Result<()> {
        self.ensure_no_active_mutations()?;
        if self.reads_safe() {
            return Ok(());
        }
        let _change = self.inner.generation_change.lock().await;
        self.ensure_no_active_mutations()?;
        self.rotate_until_safe().await
    }

    async fn rotate_until_safe(&self) -> Result<()> {
        loop {
            self.ensure_no_active_mutations()?;
            let ticket = self.inner.coherence.load(Ordering::Acquire);
            if ticket & 1 == 0 {
                return Ok(());
            }
            if self.set_fresh_generation(ticket).await? {
                return Ok(());
            }
        }
    }

    async fn set_fresh_generation(&self, ticket: u64) -> Result<bool> {
        let lease = self.manager().await?;
        let generation = lease.generation;
        let mut manager = lease.manager;
        let next = fresh_generation();
        let result: redis::RedisResult<String> = redis::cmd("SET")
            .arg(self.epoch_key())
            .arg(&next)
            .query_async(&mut manager)
            .await;
        match result {
            Ok(reply) if reply == "OK" => {
                let published = self.publish_generation(ticket);
                self.record_success().await;
                Ok(published)
            }
            Ok(_) => {
                let error = anyhow!("Redis returned an unexpected generation rotation response");
                self.handle_command_error(generation, &error).await;
                Err(error)
            }
            Err(error) => {
                let error = anyhow!(error).context("Redis cache generation rotation failed");
                self.handle_command_error(generation, &error).await;
                Err(error)
            }
        }
    }

    pub async fn snapshot(&self) -> CacheSnapshot {
        let last_success = self.inner.last_success_unix.load(Ordering::Relaxed);
        CacheSnapshot {
            provider: "redis",
            role: "discardable_public_derivative_cache",
            state: match (
                self.reads_safe() && self.active_mutations() == 0,
                self.inner.state.load(Ordering::Relaxed),
            ) {
                (false, _) => CacheState::Degraded,
                (true, 1) => CacheState::Active,
                (true, 2) => CacheState::Degraded,
                _ => CacheState::Connecting,
            },
            required: self.inner.settings.required,
            topology: match self.inner.settings.topology {
                RedisTopology::Standalone => "standalone",
                RedisTopology::Sentinel => "sentinel",
            },
            namespace: self.inner.settings.namespace.clone(),
            content_release: self.inner.settings.content_release.clone(),
            hits: self.inner.hits.load(Ordering::Relaxed),
            misses: self.inner.misses.load(Ordering::Relaxed),
            errors: self.inner.errors.load(Ordering::Relaxed),
            last_success_unix: (last_success != 0).then_some(last_success),
            last_error: self.inner.last_error.read().await.clone(),
        }
    }

    fn epoch_key(&self) -> String {
        format!(
            "{}:{CACHE_SCHEMA}:{}:epoch",
            self.inner.settings.namespace, self.inner.settings.content_release
        )
    }

    fn response_prefix(&self) -> String {
        format!(
            "{}:{CACHE_SCHEMA}:{}:response:",
            self.inner.settings.namespace, self.inner.settings.content_release
        )
    }

    fn reads_safe(&self) -> bool {
        self.inner.coherence.load(Ordering::Acquire) & 1 == 0
    }

    fn mark_dirty(&self) {
        mark_coherence_dirty(&self.inner.coherence);
    }

    fn active_mutations(&self) -> u64 {
        self.inner.active_mutations.load(Ordering::Acquire)
    }

    fn ensure_no_active_mutations(&self) -> Result<()> {
        if self.active_mutations() == 0 {
            Ok(())
        } else {
            Err(anyhow!(
                "Redis cache reads are suspended while a canonical mutation is in progress"
            ))
        }
    }

    fn publish_generation(&self, ticket: u64) -> bool {
        publish_coherence_generation(&self.inner.coherence, ticket)
    }

    async fn manager(&self) -> Result<ManagerLease> {
        if let Some(lease) = self.current_manager().await {
            return Ok(lease);
        }

        // This gate is deliberately separate from the manager slot. Sentinel
        // discovery and TCP setup may take the full configured timeout, but they
        // never retain the slot's write lock or block requests using a healthy
        // manager.
        let _attempt = self.inner.reconnect.begin_attempt()?;
        if let Some(lease) = self.current_manager().await {
            return Ok(lease);
        }

        let opened = tokio::time::timeout(
            Duration::from_millis(self.inner.settings.connect_timeout_ms),
            self.open_manager(),
        )
        .await;
        let manager = match opened {
            Ok(Ok(manager)) => manager,
            Ok(Err(_)) => {
                let error = anyhow!("Redis connection could not be established");
                self.record_connection_failure(&error).await;
                return Err(error);
            }
            Err(_) => {
                let error = anyhow!("Redis connection attempt timed out");
                self.record_connection_failure(&error).await;
                return Err(error);
            }
        };

        let mut slot = self.inner.manager.write().await;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        slot.manager = Some(manager.clone());
        let generation = slot.generation;
        drop(slot);
        Ok(ManagerLease {
            manager,
            generation,
        })
    }

    async fn open_manager(&self) -> Result<ConnectionManager> {
        let client = match self.inner.settings.topology {
            RedisTopology::Standalone => redis::Client::open(self.inner.settings.url.as_str())
                .map_err(|_| anyhow!("invalid standalone Redis endpoint"))?,
            RedisTopology::Sentinel => self.sentinel_master_client().await?,
        };
        let timeout = Duration::from_millis(self.inner.settings.connect_timeout_ms);
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(timeout))
            .set_response_timeout(Some(timeout))
            .set_min_delay(Duration::from_millis(50))
            .set_max_delay(Duration::from_secs(2))
            .set_number_of_retries(4);
        ConnectionManager::new_with_config(client, config)
            .await
            .map_err(|_| anyhow!("failed to establish Redis connection manager"))
    }

    async fn sentinel_master_client(&self) -> Result<redis::Client> {
        let connection_info = self
            .inner
            .settings
            .url
            .as_str()
            .into_connection_info()
            .map_err(|_| anyhow!("invalid Redis master connection settings"))?;
        let mut node = SentinelNodeConnectionInfo::default()
            .set_redis_connection_info(connection_info.redis_settings().clone());
        if self.inner.settings.url.scheme() == "rediss" {
            node = node.set_tls_mode(redis::TlsMode::Secure);
        }
        let sentinels = self
            .inner
            .settings
            .sentinel_urls
            .iter()
            .map(|url| url.as_str().to_owned())
            .collect::<Vec<_>>();
        let mut sentinel = SentinelClient::build(
            sentinels,
            self.inner.settings.sentinel_master.clone(),
            Some(node),
            SentinelServerType::Master,
        )
        .map_err(|_| anyhow!("invalid Redis Sentinel settings"))?;
        sentinel
            .async_get_client()
            .await
            .map_err(|_| anyhow!("Redis Sentinel could not discover the current master"))
    }

    async fn current_manager(&self) -> Option<ManagerLease> {
        let slot = self.inner.manager.read().await;
        slot.manager.clone().map(|manager| ManagerLease {
            manager,
            generation: slot.generation,
        })
    }

    /// Invalidates only the generation that observed the error. A delayed error
    /// from an old request must never remove a newly established manager.
    async fn drop_manager(&self, generation: u64) -> bool {
        let mut slot = self.inner.manager.write().await;
        if slot.generation == generation && slot.manager.is_some() {
            slot.manager = None;
            true
        } else {
            false
        }
    }

    async fn handle_command_error(&self, generation: u64, error: &anyhow::Error) {
        if self.drop_manager(generation).await {
            self.inner.reconnect.record_failure();
            self.record_error(error).await;
        } else {
            // The first failing request already changed health and opened the
            // breaker. Count subsequent failures without letting a stale request
            // overwrite the state of a newer connection.
            self.inner.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn record_connection_failure(&self, error: &anyhow::Error) {
        self.inner.reconnect.record_failure();
        self.record_error(error).await;
    }

    async fn record_success(&self) {
        self.inner.reconnect.record_success();
        self.inner.state.store(1, Ordering::Relaxed);
        self.inner
            .last_success_unix
            .store(unix_now(), Ordering::Relaxed);
        *self.inner.last_error.write().await = None;
    }

    async fn record_error(&self, error: &anyhow::Error) {
        self.inner.state.store(2, Ordering::Relaxed);
        self.inner.errors.fetch_add(1, Ordering::Relaxed);
        *self.inner.last_error.write().await = Some(redact_redis_secrets(
            &self.inner.settings,
            format!("{error:#}"),
        ));
    }
}

fn redact_redis_secrets(settings: &RedisSettings, mut message: String) -> String {
    for endpoint in std::iter::once(&settings.url).chain(settings.sentinel_urls.iter()) {
        message = message.replace(endpoint.as_str(), "[redacted Redis endpoint]");
        if let Some(password) = endpoint.password().filter(|password| !password.is_empty()) {
            message = message.replace(password, "[redacted Redis credential]");
        }
    }
    message
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn fresh_generation() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

fn mark_coherence_dirty(coherence: &AtomicU64) {
    coherence
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            let sequence = (current >> 1).wrapping_add(1);
            Some((sequence << 1) | 1)
        })
        .expect("coherence update is unconditional");
}

fn publish_coherence_generation(coherence: &AtomicU64, ticket: u64) -> bool {
    coherence
        .compare_exchange(
            ticket,
            ticket.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::Notify;

    fn disconnected_cache() -> SemanticCache {
        SemanticCache {
            inner: Arc::new(Inner {
                settings: RedisSettings {
                    topology: RedisTopology::Standalone,
                    url: url::Url::parse("redis://127.0.0.1:1/").unwrap(),
                    sentinel_urls: Vec::new(),
                    sentinel_master: "unused".into(),
                    namespace: "test".into(),
                    content_release: "test".into(),
                    required: true,
                    response_ttl_seconds: 60,
                    connect_timeout_ms: 100,
                },
                manager: RwLock::new(ManagerSlot::default()),
                reconnect: ReconnectControl::new(),
                generation_change: Mutex::new(()),
                active_mutations: AtomicU64::new(0),
                coherence: AtomicU64::new(0),
                state: AtomicU8::new(1),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                errors: AtomicU64::new(0),
                last_success_unix: AtomicU64::new(0),
                last_error: RwLock::new(None),
            }),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_attempts_are_single_flight_backed_off_and_recoverable() {
        let control = Arc::new(ReconnectControl::new());
        let attempts = Arc::new(AtomicU64::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        let first = {
            let control = Arc::clone(&control);
            let attempts = Arc::clone(&attempts);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                let permit = control.begin_attempt().expect("first attempt should start");
                attempts.fetch_add(1, Ordering::Relaxed);
                started.notify_one();
                release.notified().await;
                control.record_failure();
                drop(permit);
            })
        };

        started.notified().await;
        let contenders = (0..32)
            .map(|_| {
                let control = Arc::clone(&control);
                tokio::spawn(async move { control.begin_attempt().is_err() })
            })
            .collect::<Vec<_>>();
        for contender in contenders {
            assert!(contender.await.expect("contender task should complete"));
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 1);

        release.notify_one();
        first.await.expect("first attempt task should complete");
        assert!(control.begin_attempt().is_err(), "breaker should fail fast");

        tokio::time::sleep(RECONNECT_BACKOFF_MIN + Duration::from_millis(25)).await;
        let recovered = control
            .begin_attempt()
            .expect("retry should be admitted after backoff");
        attempts.fetch_add(1, Ordering::Relaxed);
        control.record_success();
        drop(recovered);

        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        assert!(
            control.begin_attempt().is_ok(),
            "a successful reconnect should close the breaker"
        );
    }

    #[test]
    fn health_errors_redact_all_redis_credentials_and_urls() {
        let settings = RedisSettings {
            topology: RedisTopology::Sentinel,
            url: url::Url::parse("redis://owner:primary-secret@redis.internal:6379/0")
                .expect("master URL"),
            sentinel_urls: vec![
                url::Url::parse("redis://sentinel:sentinel-secret@sentinel.internal:26379/0")
                    .expect("sentinel URL"),
            ],
            sentinel_master: "mymaster".into(),
            namespace: "osb".into(),
            content_release: "test".into(),
            required: true,
            response_ttl_seconds: 60,
            connect_timeout_ms: 250,
        };
        let message = format!(
            "failed {} using primary-secret and {} using sentinel-secret",
            settings.url, settings.sentinel_urls[0]
        );
        let redacted = redact_redis_secrets(&settings, message);

        assert!(!redacted.contains("primary-secret"));
        assert!(!redacted.contains("sentinel-secret"));
        assert!(!redacted.contains("redis.internal"));
        assert!(!redacted.contains("sentinel.internal"));
    }

    #[test]
    fn generations_do_not_repeat_or_depend_on_an_eviction_prone_counter() {
        let first = fresh_generation();
        let second = fresh_generation();
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
        assert!(!LOOKUP_SCRIPT.contains("'1'"));
        assert!(!LOOKUP_SCRIPT.contains("INCR"));
        assert!(LOOKUP_SCRIPT.contains("STRLEN"));
    }

    #[test]
    fn an_older_rotation_cannot_publish_over_a_newer_invalidation() {
        let coherence = AtomicU64::new(1);
        let old_ticket = coherence.load(Ordering::Acquire);
        mark_coherence_dirty(&coherence);
        let newest_ticket = coherence.load(Ordering::Acquire);

        assert!(!publish_coherence_generation(&coherence, old_ticket));
        assert_eq!(coherence.load(Ordering::Acquire) & 1, 1);
        assert!(publish_coherence_generation(&coherence, newest_ticket));
        assert_eq!(coherence.load(Ordering::Acquire) & 1, 0);
    }

    #[test]
    fn mutation_guard_blocks_early_repair_and_redirties_on_drop() {
        let cache = disconnected_cache();
        let mutation = cache.begin_mutation();
        assert_eq!(cache.active_mutations(), 1);
        assert!(cache.ensure_no_active_mutations().is_err());

        // Even if an in-flight older repair publishes while the mutation is
        // still running, dropping the guard creates a newer dirty ticket.
        let early_ticket = cache.inner.coherence.load(Ordering::Acquire);
        assert!(cache.publish_generation(early_ticket));
        assert!(cache.reads_safe());
        drop(mutation);

        assert_eq!(cache.active_mutations(), 0);
        assert!(!cache.reads_safe());
    }
}
