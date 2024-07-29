use hyper::header::HeaderValue;
use std::{
    convert::{TryFrom, TryInto},
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static START_TIME: AtomicU64 = AtomicU64::new(0);

/// 设置下载起始时间
pub fn set_download_start_time(t: SystemTime) {
    START_TIME.store(
        t.duration_since(UNIX_EPOCH)
            .map_or(0, |n| n.as_millis().try_into().unwrap_or(u64::MAX)),
        Relaxed,
    )
}

/// 获取下载结束之间到下载起始时间之间的时长
pub fn total_download_duration(t: SystemTime) -> Duration {
    let end_time: u64 = t
        .duration_since(UNIX_EPOCH)
        .map_or(0, |n| n.as_millis().try_into().unwrap_or(u64::MAX));
    Duration::from_millis(end_time - START_TIME.load(Relaxed))
}

pub(crate) const REQUEST_ID_HEADER: &str = "X-ReqId";

pub(crate) fn get_req_id(tn: SystemTime, tries: usize, timeout: Duration) -> HeaderValue {
    let (start_time, delta) = get_start_time_and_delta(tn);
    HeaderValue::try_from(format!(
        "r{}-{}-t{}-o{}",
        start_time,
        delta,
        tries,
        timeout.as_millis()
    ))
    .expect("Unexpected invalid header value")
}

pub(crate) fn get_req_id2(
    tn: SystemTime,
    tries: usize,
    async_task_id: u32,
    timeout: Duration,
) -> HeaderValue {
    let (start_time, delta) = get_start_time_and_delta(tn);
    HeaderValue::try_from(format!(
        "r{}-{}-t{}-a{}-o{}",
        start_time,
        delta,
        tries,
        async_task_id,
        timeout.as_millis()
    ))
    .expect("Unexpected invalid header value")
}

fn get_start_time_and_delta(tn: SystemTime) -> (u64, u128) {
    let start_time: u64 = START_TIME.load(Relaxed);
    let end_time: u128 = tn.duration_since(UNIX_EPOCH).map_or(0, |n| n.as_nanos());
    let delta: u128 = end_time - u128::from(start_time) * 1000 * 1000;
    (start_time, delta)
}
