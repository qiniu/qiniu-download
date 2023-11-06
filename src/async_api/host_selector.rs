use super::dot::Dotter;
use log::info;
use rand::{seq::SliceRandom, thread_rng};
use scc::HashMap;
use std::{
    cmp::{min, Ordering},
    collections::HashSet,
    fmt::{Debug, Formatter, Result as FormatResult},
    future::Future,
    io::{Error as IoError, Result as IoResult},
    ops::Deref,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Arc,
    },
    time::{Duration, Instant},
};
use tap::prelude::*;
use tokio::{
    spawn,
    sync::{Mutex, RwLock},
};

#[derive(Default, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
struct OptionalInstantTime(Option<Instant>);

impl Deref for OptionalInstantTime {
    type Target = Option<Instant>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl OptionalInstantTime {
    fn now() -> Self {
        Self(Some(Instant::now()))
    }
}

#[derive(Default, Clone, Debug, Eq, PartialEq)]
struct PunishedInfo {
    last_punished_at: OptionalInstantTime,
    continuous_punished_times: usize,
    timeout_power: usize,
    failed_to_connect: bool,
}

impl Ord for PunishedInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.failed_to_connect != other.failed_to_connect {
            return self.failed_to_connect.cmp(&other.failed_to_connect);
        }
        if self.timeout_power != other.timeout_power {
            return self.timeout_power.cmp(&other.timeout_power);
        }
        if self.continuous_punished_times != other.continuous_punished_times {
            return self
                .continuous_punished_times
                .cmp(&other.continuous_punished_times);
        }
        self.last_punished_at.cmp(&other.last_punished_at)
    }
}

impl PartialOrd for PunishedInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
struct Candidate<'a> {
    host: &'a str,
    punish_duration: Duration,
    max_punished_times: usize,
    punished_info: PunishedInfo,
}

impl<'a> Eq for Candidate<'a> {}
impl<'a> PartialEq for Candidate<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.punished_info == other.punished_info
            && self.punish_duration == other.punish_duration
            && self.max_punished_times == other.max_punished_times
    }
}

impl<'a> Ord for Candidate<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.is_punishment_expired(), other.is_punishment_expired()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => match (self.is_available(), other.is_available()) {
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => other.punished_info.cmp(&self.punished_info),
            },
        }
    }
}

impl<'a> PartialOrd for Candidate<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Candidate<'a> {
    fn is_punishment_expired(&self) -> bool {
        if let Some(last_punished_at) = self.punished_info.last_punished_at.as_ref() {
            last_punished_at.elapsed() >= self.punish_duration
        } else {
            true
        }
    }

    fn is_available(&self) -> bool {
        !self.punished_info.failed_to_connect
            && self.punished_info.continuous_punished_times <= self.max_punished_times
    }
}

type UpdateFn = Box<
    dyn Fn() -> Pin<Box<dyn Future<Output = IoResult<Vec<String>>> + Send + Sync + 'static>>
        + Sync
        + Send
        + 'static,
>;

struct HostsUpdater {
    hosts: RwLock<Vec<String>>,
    hosts_map: HashMap<String, PunishedInfo>,
    update_option: Option<UpdateOption>,
    index: AtomicUsize,
    current_timeout_power: AtomicUsize,
}

struct UpdateOption {
    func: UpdateFn,
    interval: Duration,
    last_updated_at: Mutex<Instant>,
}

impl UpdateOption {
    fn new(func: UpdateFn, interval: Duration) -> Self {
        Self {
            func,
            interval,
            last_updated_at: Mutex::new(Instant::now()),
        }
    }
}

impl HostsUpdater {
    async fn new(hosts: Vec<String>, update_option: Option<UpdateOption>) -> Arc<Self> {
        let hosts_map = HashMap::default();
        for host in &hosts {
            hosts_map
                .insert_async(host.to_owned(), Default::default())
                .await
                .ok();
        }
        Arc::new(Self {
            hosts_map,
            update_option,
            hosts: RwLock::new(hosts),
            index: AtomicUsize::new(0),
            current_timeout_power: AtomicUsize::new(0),
        })
    }

    async fn set_hosts(&self, mut hosts: Vec<String>) {
        let mut new_hosts_set = HashSet::with_capacity(hosts.len());
        for host in hosts.iter() {
            new_hosts_set.insert(host.to_owned());
            self.hosts_map
                .entry_async(host.to_owned())
                .await
                .and_modify(|v| *v = Default::default())
                .or_default();
        }
        self.hosts_map
            .retain_async(|host, _| new_hosts_set.contains(host))
            .await;
        hosts.shuffle(&mut thread_rng());
        *self.hosts.write().await = hosts;
    }

    async fn update_hosts(&self) -> bool {
        if let Some(update_option) = &self.update_option {
            if let Ok(new_hosts) = (update_option.func)().await {
                if !new_hosts.is_empty() {
                    self.set_hosts(new_hosts).await;
                    return true;
                }
            }
        }
        false
    }

    fn next_index(updater: &Arc<HostsUpdater>) -> usize {
        return updater.index.fetch_add(1, Relaxed).tap(|_| {
            try_to_auto_update(updater);
        });

        fn try_to_auto_update(updater: &Arc<HostsUpdater>) {
            if let Some(update_option) = &updater.update_option {
                if let Ok(last_updated_at) = update_option.last_updated_at.try_lock() {
                    if last_updated_at.elapsed() >= update_option.interval {
                        let updater = updater.to_owned();
                        drop(last_updated_at);
                        spawn(async move { try_to_auto_update_in_thread(updater).await });
                    }
                }
            }
        }

        async fn try_to_auto_update_in_thread(updater: Arc<HostsUpdater>) {
            if let Some(update_option) = &updater.update_option {
                let mut last_updated_at = update_option.last_updated_at.lock().await;
                if last_updated_at.elapsed() >= update_option.interval {
                    if updater.update_hosts().await {
                        info!("`host-selector-auto-updater` update hosts successfully");
                    };
                    *last_updated_at = Instant::now();
                }
            }
        }
    }

    pub(super) async fn increase_timeout_power_by(&self, host: &str, mut timeout_power: usize) {
        self.hosts_map
            .update_async(host, |_, punished_info| {
                timeout_power = timeout_power.saturating_add(1);
                if punished_info.timeout_power < timeout_power {
                    punished_info.timeout_power = timeout_power;
                    info!(
                        "The timeout_power of host {} increases, now is {}",
                        host, punished_info.timeout_power
                    );
                }
                punished_info.last_punished_at = OptionalInstantTime::now();
            })
            .await;
    }

    pub(super) async fn mark_connection_as_failed(&self, host: &str) {
        self.hosts_map
            .update_async(host, |_, punished_info| {
                punished_info.failed_to_connect = true;
                punished_info.last_punished_at = OptionalInstantTime::now();
            })
            .await;
    }
}

impl Debug for HostsUpdater {
    fn fmt(&self, f: &mut Formatter<'_>) -> FormatResult {
        f.debug_struct("HostsUpdater").finish()
    }
}

type ShouldPunishFn = Box<
    dyn Fn(&IoError) -> Pin<Box<dyn Future<Output = bool> + Send + Sync + 'static>>
        + Send
        + Sync
        + 'static,
>;

struct HostPunisher {
    should_punish_func: Option<ShouldPunishFn>,
    punish_duration: Duration,
    base_timeout: Duration,
    max_punished_times: usize,
    max_punished_hosts_percent: u8,
}

impl HostPunisher {
    fn max_seek_times(&self, hosts_count: usize) -> usize {
        hosts_count * usize::from(self.max_punished_hosts_percent) / 100
    }

    fn is_available(&self, punished_info: &PunishedInfo, connection_sensitive: bool) -> bool {
        if connection_sensitive && punished_info.failed_to_connect {
            return false;
        }
        punished_info.continuous_punished_times <= self.max_punished_times
    }

    fn is_punishment_expired(&self, punished_info: &PunishedInfo) -> bool {
        if let Some(last_punished_at) = punished_info.last_punished_at.as_ref() {
            last_punished_at.elapsed() >= self.punish_duration
        } else {
            true
        }
    }

    fn timeout(&self, punished_info: &PunishedInfo) -> Duration {
        min(
            // 超时时长有上限，否则可能超过 tokio 极限
            self.base_timeout * (1 << punished_info.timeout_power),
            Duration::from_secs(600),
        )
    }

    async fn should_punish(&self, error: &IoError) -> bool {
        if let Some(should_punish_func) = &self.should_punish_func {
            should_punish_func(error).await
        } else {
            true
        }
    }
}

impl Debug for HostPunisher {
    fn fmt(&self, f: &mut Formatter<'_>) -> FormatResult {
        f.debug_struct("HostPunisher")
            .field("should_punish", &self.should_punish_func.is_some())
            .field("punish_duration", &self.punish_duration)
            .field("base_timeout", &self.base_timeout)
            .field("max_punished_times", &self.max_punished_times)
            .field(
                "max_punished_hosts_percent",
                &self.max_punished_hosts_percent,
            )
            .finish()
    }
}

#[derive(Debug, Clone)]
pub(super) struct HostSelector {
    hosts_updater: Arc<HostsUpdater>,
    host_punisher: Arc<HostPunisher>,
}

pub(super) struct HostSelectorBuilder {
    hosts: Vec<String>,
    update_func: Option<UpdateFn>,
    should_punish_func: Option<ShouldPunishFn>,
    update_interval: Duration,
    punish_duration: Duration,
    base_timeout: Duration,
    max_punished_times: usize,
    max_punished_hosts_percent: u8,
}

impl HostSelectorBuilder {
    pub(super) fn new(hosts: Vec<String>) -> Self {
        Self {
            hosts,
            update_func: None,
            should_punish_func: None,
            update_interval: Duration::from_secs(60),
            punish_duration: Duration::from_secs(30 * 60),
            base_timeout: Duration::from_millis(3000),
            max_punished_times: 5,
            max_punished_hosts_percent: 50,
        }
    }

    pub(super) fn update_callback(mut self, update_func: Option<UpdateFn>) -> Self {
        self.update_func = update_func;
        self
    }

    pub(super) fn should_punish_callback(
        mut self,
        should_punish_func: Option<ShouldPunishFn>,
    ) -> Self {
        self.should_punish_func = should_punish_func;
        self
    }

    pub(super) fn update_interval(mut self, interval: Duration) -> Self {
        self.update_interval = interval;
        self
    }

    pub(super) fn punish_duration(mut self, duration: Duration) -> Self {
        self.punish_duration = duration;
        self
    }

    pub(super) fn base_timeout(mut self, timeout: Duration) -> Self {
        self.base_timeout = timeout;
        self
    }

    pub(super) fn max_punished_times(mut self, times: usize) -> Self {
        self.max_punished_times = times;
        self
    }

    pub(super) fn max_punished_hosts_percent(mut self, percent: u8) -> Self {
        self.max_punished_hosts_percent = percent;
        self
    }

    pub(super) async fn build(self) -> HostSelector {
        let auto_update_enabled = self.update_func.is_some();
        let is_hosts_empty = self.hosts.is_empty();
        let update_interval = self.update_interval;
        let hosts_updater = HostsUpdater::new(
            self.hosts,
            self.update_func
                .map(|f| UpdateOption::new(f, update_interval)),
        )
        .await;

        if auto_update_enabled && is_hosts_empty {
            hosts_updater.update_hosts().await;
        }

        HostSelector {
            hosts_updater,
            host_punisher: Arc::new(HostPunisher {
                should_punish_func: self.should_punish_func,
                punish_duration: self.punish_duration,
                base_timeout: self.base_timeout,
                max_punished_times: self.max_punished_times,
                max_punished_hosts_percent: self.max_punished_hosts_percent,
            }),
        }
    }
}

impl HostSelector {
    pub(super) fn builder(hosts: Vec<String>) -> HostSelectorBuilder {
        HostSelectorBuilder::new(hosts)
    }

    pub(super) async fn set_hosts(&self, hosts: Vec<String>) {
        self.hosts_updater.set_hosts(hosts).await
    }

    pub(super) async fn hosts(&self) -> Vec<String> {
        self.hosts_updater
            .hosts
            .read()
            .await
            .iter()
            .filter(|&host| {
                self.hosts_updater
                    .hosts_map
                    .read(host, |_, punished_info| {
                        self.host_punisher.is_punishment_expired(punished_info)
                            || self.host_punisher.is_available(punished_info, true)
                    })
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }

    pub(super) async fn all_hosts_crc32(&self) -> u32 {
        let mut hosts = self.hosts_updater.hosts.read().await.to_owned();
        hosts.sort();
        let mut hasher = crc32fast::Hasher::new();
        hosts.iter().enumerate().for_each(|(i, host)| {
            if i > 0 {
                hasher.update(b"$");
            }
            hasher.update(host.as_bytes());
        });
        hasher.finalize()
    }

    pub(super) async fn update_hosts(&self) -> bool {
        if self.hosts_updater.update_hosts().await {
            info!("manual update hosts successfully");
            if let Some(update_option) = self.hosts_updater.update_option.as_ref() {
                *update_option.last_updated_at.lock().await = Instant::now();
            }
            true
        } else {
            false
        }
    }

    pub(super) async fn select_host(&self, tried: &HashSet<String>) -> Option<HostInfo> {
        struct CurrentHostInfo<'a> {
            host: &'a str,
            timeout: Duration,
            timeout_power: usize,
        }
        let mut chosen_host_info = None;

        let hosts = self.hosts_updater.hosts.read().await;
        let max_seek_times = self.host_punisher.max_seek_times(hosts.len());
        let mut candidates = Vec::with_capacity(max_seek_times + 1);
        for _ in 0..=max_seek_times {
            let index = HostsUpdater::next_index(&self.hosts_updater);
            let host = hosts[index % hosts.len()].as_str();
            if tried.contains(host) {
                continue;
            } else if let Some(true) = self.hosts_updater.hosts_map.read_async(host, |_, punished_info| {
                if self.host_punisher.is_punishment_expired(punished_info) {
                    info!("host {} is selected directly because there is no punishment or punishment is expired, timeout: {:?}", host,self.host_punisher.base_timeout);
                    chosen_host_info = Some(CurrentHostInfo {
                        host,
                        timeout: self.host_punisher.base_timeout,
                        timeout_power: 0,
                    });
                    true
                } else if self.is_satisfied_with(punished_info) {
                    info!(
                        "host {} is selected, timeout: {:?}, timeout power: {:?}",
                        host,
                        self.host_punisher.timeout(punished_info),
                        punished_info.timeout_power,
                    );
                    chosen_host_info = Some(CurrentHostInfo {
                        host,
                        timeout: self.host_punisher.timeout(punished_info),
                        timeout_power: punished_info.timeout_power,
                    });
                    true
                } else {
                    info!(
                        "host {} is unsatisfied, put it into candidates, timeout: {:?}, timeout power: {:?}",
                        host,
                        self.host_punisher.timeout(punished_info),
                        punished_info.timeout_power,
                    );
                    candidates.push(Candidate {
                        host,
                        punish_duration: self.host_punisher.punish_duration,
                        max_punished_times: self.host_punisher.max_punished_times,
                        punished_info: punished_info.to_owned(),
                    });
                    false
                }
            }).await {
                break;
            }
        }
        chosen_host_info
            .or_else(|| {
                candidates
                    .into_iter()
                    .max()
                    .map(|c| CurrentHostInfo {
                        host: c.host,
                        timeout: self.host_punisher.timeout(&c.punished_info),
                        timeout_power: c.punished_info.timeout_power,
                    })
                    .tap_some(|c| {
                        info!(
                            "candidate_host {} is selected, timeout: {:?}, timeout power: {:?}",
                            c.host, c.timeout, c.timeout_power,
                        );
                    })
            })
            .map(|chosen_host_info| {
                self.hosts_updater
                    .current_timeout_power
                    .store(chosen_host_info.timeout_power, Relaxed);
                HostInfo {
                    host: chosen_host_info.host.to_owned(),
                    timeout: chosen_host_info.timeout,
                    timeout_power: chosen_host_info.timeout_power,
                }
            })
    }

    pub(super) async fn reward(&self, host: &str) {
        self.hosts_updater
            .hosts_map
            .update_async(host, |_, punished_info| {
                punished_info.continuous_punished_times = 0;
                punished_info.failed_to_connect = false;
                punished_info.timeout_power = punished_info.timeout_power.saturating_sub(1);
                info!(
                    "Reward host {}, now timeout_power is {}",
                    host, punished_info.timeout_power
                );
            })
            .await;
    }

    pub(super) async fn punish(&self, host: &str, error: &IoError, dotter: &Dotter) -> bool {
        match self.punish_without_dotter(host, error).await {
            PunishResult::NoPunishment => false,
            PunishResult::Punished => true,
            PunishResult::PunishedAndFreezed => {
                dotter.punish().await.ok();
                true
            }
        }
    }

    pub(super) async fn punish_without_dotter(&self, host: &str, error: &IoError) -> PunishResult {
        if self.host_punisher.should_punish(error).await {
            let result = self
                .hosts_updater
                .hosts_map
                .update_async(host, |_, punished_info| {
                    punished_info.continuous_punished_times += 1;
                    punished_info.last_punished_at = OptionalInstantTime::now();
                    info!(
                    "Punish host {}, now continuous_punished_times is {}, and timeout_power is {}",
                    host, punished_info.continuous_punished_times, punished_info.timeout_power
                );

                    if self.host_punisher.is_available(punished_info, false) {
                        None
                    } else {
                        Some(PunishResult::PunishedAndFreezed)
                    }
                })
                .await
                .flatten();
            result.unwrap_or(PunishResult::Punished)
        } else {
            PunishResult::NoPunishment
        }
    }

    pub(super) async fn increase_timeout_power_by(&self, host: &str, timeout_power: usize) {
        self.hosts_updater
            .increase_timeout_power_by(host, timeout_power)
            .await
    }

    pub(super) async fn mark_connection_as_failed(&self, host: &str) {
        self.hosts_updater.mark_connection_as_failed(host).await
    }

    pub(super) fn base_timeout(&self) -> Duration {
        self.host_punisher.base_timeout
    }

    fn is_satisfied_with(&self, punished_info: &PunishedInfo) -> bool {
        self.host_punisher.is_available(punished_info, true)
            && self.hosts_updater.current_timeout_power.load(Relaxed) >= punished_info.timeout_power
    }
}

pub(super) enum PunishResult {
    NoPunishment,
    Punished,
    PunishedAndFreezed,
}

#[derive(Debug, Clone, Default)]
pub(super) struct HostInfo {
    host: String,
    timeout_power: usize,
    timeout: Duration,
}

impl HostInfo {
    pub(super) fn host(&self) -> &str {
        &self.host
    }

    pub(super) fn timeout_power(&self) -> usize {
        self.timeout_power
    }

    pub(super) fn timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::sleep;

    use super::*;
    use std::io::ErrorKind as IoErrorKind;

    #[tokio::test]
    async fn test_hosts_updater() {
        env_logger::try_init().ok();

        let hosts_updater = HostsUpdater::new(
            vec![
                "http://host1".to_owned(),
                "http://host2".to_owned(),
                "http://host3".to_owned(),
            ],
            Some(UpdateOption::new(
                Box::new(|| {
                    Box::pin(async {
                        Ok(vec![
                            "http://host1".to_owned(),
                            "http://host2".to_owned(),
                            "http://host4".to_owned(),
                            "http://host5".to_owned(),
                        ])
                    })
                }),
                Duration::from_secs(10),
            )),
        )
        .await;
        assert_eq!(hosts_updater.hosts.read().await.len(), 3);
        assert_eq!(hosts_updater.hosts_map.len(), 3);
        assert!(hosts_updater.update_hosts().await);
        assert_eq!(hosts_updater.hosts.read().await.len(), 4);
        assert_eq!(hosts_updater.hosts_map.len(), 4);
        assert!(hosts_updater.hosts_map.contains_async("http://host4").await);
        assert!(hosts_updater.hosts_map.contains_async("http://host5").await);
        assert!(!hosts_updater.hosts_map.contains_async("http://host3").await);
    }

    #[tokio::test]
    async fn test_hosts_update() {
        env_logger::try_init().ok();

        let host_selector = HostSelectorBuilder::new(vec![])
            .update_callback(Some(Box::new(|| {
                Box::pin(async {
                    Ok(vec![
                        "http://host1".to_owned(),
                        "http://host2".to_owned(),
                        "http://host4".to_owned(),
                        "http://host5".to_owned(),
                    ])
                })
            })))
            .build()
            .await;
        let selected_host = host_selector
            .select_host(&Default::default())
            .await
            .unwrap()
            .host;
        assert!([
            "http://host1".to_owned(),
            "http://host2".to_owned(),
            "http://host4".to_owned(),
            "http://host5".to_owned(),
        ]
        .contains(&selected_host))
    }

    #[tokio::test]
    async fn test_hosts_updater_auto_update() {
        env_logger::try_init().ok();

        let hosts_updater = HostsUpdater::new(
            vec![
                "http://host1".to_owned(),
                "http://host2".to_owned(),
                "http://host3".to_owned(),
            ],
            Some(UpdateOption::new(
                Box::new(|| {
                    Box::pin(async {
                        Ok(vec![
                            "http://host1".to_owned(),
                            "http://host2".to_owned(),
                            "http://host4".to_owned(),
                            "http://host5".to_owned(),
                        ])
                    })
                }),
                Duration::from_millis(500),
            )),
        )
        .await;
        HostsUpdater::next_index(&hosts_updater);
        assert_eq!(hosts_updater.hosts.read().await.len(), 3);
        assert_eq!(hosts_updater.hosts_map.len(), 3);
        sleep(Duration::from_millis(500)).await;
        HostsUpdater::next_index(&hosts_updater);
        sleep(Duration::from_millis(500)).await;
        assert_eq!(hosts_updater.hosts.read().await.len(), 4);
        assert_eq!(hosts_updater.hosts_map.len(), 4);
        assert!(hosts_updater.hosts_map.contains_async("http://host4").await);
        assert!(hosts_updater.hosts_map.contains_async("http://host5").await);
        assert!(!hosts_updater.hosts_map.contains_async("http://host3").await);
    }

    #[tokio::test]
    async fn test_hosts_selector() {
        env_logger::try_init().ok();

        let punished_errs = Arc::new(Mutex::new(Vec::new()));
        {
            let host_selector = HostSelectorBuilder::new(vec![
                "http://host1".to_owned(),
                "http://host2".to_owned(),
                "http://host3".to_owned(),
            ])
            .should_punish_callback(Some({
                let punished_errs = punished_errs.to_owned();
                Box::new(move |error| {
                    let error = error.to_string();
                    let punished_errs = punished_errs.to_owned();
                    Box::pin(async move {
                        punished_errs.lock().await.push(error);
                        true
                    })
                })
            }))
            .punish_duration(Duration::from_millis(500))
            .base_timeout(Duration::from_millis(100))
            .max_punished_times(2)
            .build()
            .await;

            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host1".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            assert_eq!(
                host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap()
                    .host,
                "http://host2".to_owned()
            );
            assert_eq!(
                host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap()
                    .host,
                "http://host3".to_owned()
            );
            assert_eq!(
                host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap()
                    .host,
                "http://host1".to_owned()
            );
            host_selector
                .increase_timeout_power_by("http://host1", 0)
                .await;
            host_selector
                .punish(
                    "http://host1",
                    &IoError::new(IoErrorKind::Other, "err1"),
                    &Default::default(),
                )
                .await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            host_selector
                .punish(
                    "http://host1",
                    &IoError::new(IoErrorKind::Other, "err2"),
                    &Default::default(),
                )
                .await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            host_selector
                .increase_timeout_power_by("http://host1", 1)
                .await;
            host_selector
                .punish(
                    "http://host1",
                    &IoError::new(IoErrorKind::Other, "err3"),
                    &Default::default(),
                )
                .await;
            assert_eq!(
                host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap()
                    .host,
                "http://host3".to_owned()
            );
            host_selector
                .punish(
                    "http://host2",
                    &IoError::new(IoErrorKind::Other, "err4"),
                    &Default::default(),
                )
                .await;
            assert_eq!(
                host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap()
                    .host,
                "http://host2".to_owned()
            );
            host_selector
                .increase_timeout_power_by("http://host2", 0)
                .await;
            host_selector
                .punish(
                    "http://host2",
                    &IoError::new(IoErrorKind::Other, "err5"),
                    &Default::default(),
                )
                .await;
            host_selector
                .increase_timeout_power_by("http://host3", 1)
                .await;
            host_selector
                .punish(
                    "http://host3",
                    &IoError::new(IoErrorKind::Other, "err6"),
                    &Default::default(),
                )
                .await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(400));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(200));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(400));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(200));
            }
            host_selector
                .increase_timeout_power_by("http://host3", 2)
                .await;
            host_selector
                .punish(
                    "http://host3",
                    &IoError::new(IoErrorKind::Other, "err7"),
                    &Default::default(),
                )
                .await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(800));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(200));
            }
            host_selector.reward("http://host1").await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host1".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(200));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(200));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host1".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(200));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(200));
            }
            sleep(Duration::from_millis(500)).await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host1".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            host_selector
                .increase_timeout_power_by("http://host3", 2)
                .await;
            host_selector
                .punish(
                    "http://host3",
                    &IoError::new(IoErrorKind::Other, "err8"),
                    &Default::default(),
                )
                .await;
            host_selector
                .punish(
                    "http://host3",
                    &IoError::new(IoErrorKind::Other, "err9"),
                    &Default::default(),
                )
                .await;
            host_selector
                .punish(
                    "http://host3",
                    &IoError::new(IoErrorKind::Other, "err10"),
                    &Default::default(),
                )
                .await;
            host_selector
                .increase_timeout_power_by("http://host1", 3)
                .await;
            host_selector
                .punish(
                    "http://host1",
                    &IoError::new(IoErrorKind::Other, "err11"),
                    &Default::default(),
                )
                .await;
            host_selector
                .punish(
                    "http://host1",
                    &IoError::new(IoErrorKind::Other, "err12"),
                    &Default::default(),
                )
                .await;
            host_selector
                .punish(
                    "http://host1",
                    &IoError::new(IoErrorKind::Other, "err13"),
                    &Default::default(),
                )
                .await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(800));
            }
            host_selector
                .increase_timeout_power_by("http://host3", 3)
                .await;
            host_selector
                .punish(
                    "http://host3",
                    &IoError::new(IoErrorKind::Other, "err14"),
                    &Default::default(),
                )
                .await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host1".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(1600));
            }
            host_selector.reward("http://host3").await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(800));
            }
            host_selector
                .mark_connection_as_failed("http://host2")
                .await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(800));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host1".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(1600));
            }
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(800));
            }
            host_selector.reward("http://host2").await;
            {
                let host_info = host_selector
                    .select_host(&Default::default())
                    .await
                    .unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
            }
            {
                let mut tried = HashSet::new();
                tried.insert("http://host1".to_owned());
                let host_info = host_selector.select_host(&tried).await.unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(800));
                let host_info = host_selector.select_host(&tried).await.unwrap();
                assert_eq!(host_info.host, "http://host2".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(100));
                let host_info = host_selector.select_host(&tried).await.unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(800));
                tried.insert("http://host2".to_owned());
                let host_info = host_selector.select_host(&tried).await.unwrap();
                assert_eq!(host_info.host, "http://host3".to_owned());
                assert_eq!(host_info.timeout, Duration::from_millis(800));
                tried.insert("http://host3".to_owned());
                assert!(host_selector.select_host(&tried).await.is_none());
            }
        }
        assert_eq!(
            Arc::try_unwrap(punished_errs).unwrap().into_inner().len(),
            14
        );
    }
}
