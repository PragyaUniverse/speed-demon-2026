use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs;

fn main() {
    let data = env::var("SD_DATA").unwrap_or("/data".into());
    let out = env::var("SD_OUT").unwrap_or("/out".into());
    let input = fs::read_to_string(format!("{data}/requests.log")).unwrap();
    let mut groups: HashMap<(&str, &str), (Vec<u32>, u64)> = HashMap::new();
    for line in input.lines() {
        let mut f = line.split(' ');
        let ts = f.next().unwrap();
        let endpoint = f.nth(1).unwrap();
        let status: u32 = f.next().unwrap().parse().unwrap();
        let latency: u32 = f.next().unwrap().parse().unwrap();
        let group = groups.entry((&ts[..13], endpoint)).or_default();
        group.0.push(latency);
        group.1 += (500..=599).contains(&status) as u64;
    }
    let mut keys: Vec<_> = groups.keys().copied().collect();
    keys.sort_unstable();
    let mut text = String::new();
    for key in keys {
        let (lat, errors) = groups.get_mut(&key).unwrap();
        lat.sort_unstable();
        let n = lat.len();
        let pct = |p: usize| lat[(p * n + 99) / 100 - 1];
        writeln!(
            text,
            "{}:00:00Z {} {} {} {} {} {} {}",
            key.0,
            key.1,
            n,
            pct(50),
            pct(95),
            pct(99),
            errors,
            *errors * 1_000_000 / n as u64
        )
        .unwrap();
    }
    fs::write(format!("{out}/summary.txt"), text).unwrap();
}
