//! Shopping Sessions: dirty click-stream CSV in, two exact daily Parquet tables out.
//!
//! Phases (all on NCPU threads):
//!   1. links parsed; then one thread builds the person names while the others
//!      parse events (scattered by event_id) and clicks
//!   2. per event_id partition: dedup on the smallest ingest_ts, then the kept,
//!      surviving copies go to their person's partition; clicks likewise
//!   3. per person partition: sessions, credit, assists, daily aggregates
//!   4. Parquet

mod arena;
mod csv;
mod events;
mod hash;
mod input;
mod key;
mod links;
mod lookups;
mod output;
mod sessions;
mod stats;
mod ts;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use arena::Scatter;
use parquet::data_type::ByteArray;

use events::{Click, Context, EventCopy};
use output::Column;
use sessions::{Acc, PersonClick, PersonEvent, Shape, NO_CATEGORY};
use ts::DAY;

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn main() {
    if std::env::var_os("SS_NOOP").is_some() {
        std::process::exit(0); // measures the fixed cost of starting and stopping the container
    }
    let mut stats = stats::Stats::new();
    let mut log = |what: &str| stats.log(what);
    let args: Vec<String> = std::env::args().collect();
    let (data, out) = if args.len() >= 3 {
        (PathBuf::from(&args[1]), PathBuf::from(&args[2]))
    } else {
        (
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into())),
            PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into())),
        )
    };
    let threads = env_usize("NCPU")
        .or_else(|| std::thread::available_parallelism().ok().map(usize::from))
        .unwrap_or(4)
        .max(1);
    let chunk_size = env_usize("SS_CHUNK").unwrap_or(4 << 20).max(1);

    let window = lookups::read_manifest(&data);
    let products = lookups::Products::read(&data);
    let campaigns = lookups::Campaigns::read(&data);
    let event_input = input::Input::open(&data.join("events"));
    let click_input = input::Input::open(&data.join("clicks"));
    let link_input = input::Input::open(&data.join("links"));
    let event_bytes = event_input.bytes() as usize;
    let click_bytes = click_input.bytes() as usize;
    let simd = input::use_simd();
    let est_copies = event_bytes / 75 + 1;
    let eid_parts = env_usize("SS_PARTS").unwrap_or((est_copies / 16_384).clamp(1, 4096)).max(1);
    let person_parts = env_usize("SS_PARTS").unwrap_or((est_copies / 32_768).clamp(1, 4096)).max(1);
    log(&format!(
        "threads {threads}, simd {simd}, events {event_bytes} B in {} files, clicks {click_bytes} B, {} link files, partitions {eid_parts}/{person_parts}",
        event_input.files.len(),
        link_input.files.len()
    ));

    let cx = Context {
        window_start: window.start,
        window_end: window.end,
        products: &products,
        campaigns: &campaigns,
    };

    // Phase 1a: links.
    let link_chunks = link_input.chunks(chunk_size);
    let next = AtomicUsize::new(0);
    let link_pairs: Vec<Vec<(u64, u64)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (link_chunks, link_input, next) = (&link_chunks, &link_input, &next);
                s.spawn(move || {
                    let mut out = Vec::new();
                    let mut buf = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(c) = link_chunks.get(i) else { break };
                        link_input.parse::<2, _>(c, &mut buf, simd, &mut |f| events::link_fields(f, &mut out));
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    log(&format!("links parsed: {} pairs", link_pairs.iter().map(Vec::len).sum::<usize>()));

    // Phase 1b: names on thread 0, events and clicks everywhere.
    let event_chunks = event_input.chunks(chunk_size);
    let click_chunks = click_input.chunks(chunk_size);
    let total_chunks = event_chunks.len() + click_chunks.len();
    let reserve = est_copies / (threads * eid_parts) * 5 / 4 + 16;
    let est_clicks = click_bytes / 40 + 1;
    let click_reserve = est_clicks / threads * 5 / 4 + 16;
    let next = AtomicUsize::new(0);
    let mut names = None;
    let mut event_parts: Vec<Scatter<EventCopy>> = Vec::with_capacity(threads);
    let mut click_bufs: Vec<Scatter<Click>> = Vec::with_capacity(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (event_chunks, click_chunks, event_input, click_input, next, cx, link_pairs) =
                    (&event_chunks, &click_chunks, &event_input, &click_input, &next, &cx, &link_pairs);
                s.spawn(move || {
                    let names = (t == 0).then(|| links::build(link_pairs));
                    let mut parts: Scatter<EventCopy> = Scatter::new(eid_parts, reserve);
                    let mut clicks: Scatter<Click> = Scatter::new(1, click_reserve);
                    let mut buf = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= total_chunks {
                            break;
                        }
                        if i < event_chunks.len() {
                            event_input.parse::<7, _>(&event_chunks[i], &mut buf, simd, &mut |f| {
                                events::event_fields(f, cx, &mut parts)
                            });
                        } else {
                            click_input.parse::<4, _>(&click_chunks[i - event_chunks.len()], &mut buf, simd, &mut |f| {
                                events::click_fields(f, cx, &mut clicks)
                            });
                        }
                    }
                    (names, parts, clicks)
                })
            })
            .collect();
        for h in handles {
            let (n, parts, clicks) = h.join().unwrap();
            if n.is_some() {
                names = n;
            }
            event_parts.push(parts);
            click_bufs.push(clicks);
        }
    });
    let names = names.expect("names were built");
    drop(link_pairs);
    log(&format!(
        "events parsed: {} copies, {} valid clicks",
        event_parts.iter().map(Scatter::total).sum::<usize>(),
        click_bufs.iter().map(Scatter::total).sum::<usize>()
    ));

    // Phase 2: dedup and route to person partitions.
    const CLICK_SLICE: usize = 1 << 16;
    let click_items: Vec<&[Click]> = click_bufs
        .iter()
        .flat_map(|b| b.part(0))
        .flat_map(|slice| slice.chunks(CLICK_SLICE))
        .collect();
    let person_reserve = est_copies / (threads * person_parts) * 5 / 4 + 16;
    let person_click_reserve = est_clicks / (threads * person_parts) * 5 / 4 + 16;
    let next = AtomicUsize::new(0);
    let next_click = AtomicUsize::new(0);
    let (person_events, person_clicks): (Vec<Scatter<PersonEvent>>, Vec<Scatter<PersonClick>>) =
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let (event_parts, click_items, names, next, next_click) =
                        (&event_parts, &click_items, &names, &next, &next_click);
                    s.spawn(move || {
                        let mut evs: Scatter<PersonEvent> = Scatter::new(person_parts, person_reserve);
                        let mut cls: Scatter<PersonClick> = Scatter::new(person_parts, person_click_reserve);
                        let mut table = Vec::new();
                        let mut kept = Vec::new();
                        let mut slices: Vec<&[EventCopy]> = Vec::with_capacity(2 * event_parts.len());
                        loop {
                            let p = next.fetch_add(1, Ordering::Relaxed);
                            if p >= eid_parts {
                                break;
                            }
                            slices.clear();
                            slices.extend(event_parts.iter().flat_map(|t| t.part(p)));
                            sessions::dedup(&slices, names, &mut table, &mut kept, &mut evs);
                        }
                        loop {
                            let i = next_click.fetch_add(1, Ordering::Relaxed);
                            let Some(&slice) = click_items.get(i) else { break };
                            sessions::route_clicks(slice, names, &mut cls);
                        }
                        (evs, cls)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).unzip()
        });
    log("deduplicated");

    // Phase 3: sessions per person partition, biggest first.
    let first_day = window.start.div_euclid(DAY);
    let last_day = (window.end - 1).div_euclid(DAY);
    let shape = Shape {
        first_day,
        days: (last_day - first_day + 1).max(1) as usize,
        campaigns: campaigns.ids.len(),
    };
    let mut order: Vec<(usize, usize)> = (0..person_parts)
        .map(|q| {
            let n = person_events.iter().map(|t| t.len(q)).sum::<usize>()
                + person_clicks.iter().map(|t| t.len(q)).sum::<usize>();
            (n, q)
        })
        .collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let next = AtomicUsize::new(0);
    let accs: Vec<Acc> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (order, person_events, person_clicks, next) = (&order, &person_events, &person_clicks, &next);
                s.spawn(move || {
                    let mut acc = Acc::new(shape);
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(_, q)) = order.get(i) else { break };
                        acc.partition(
                            person_events.iter().flat_map(|t| t.part(q)),
                            person_clicks.iter().flat_map(|t| t.part(q)),
                        );
                    }
                    acc
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    log("sessionized");

    // Phase 4: output.
    let user_dir = out.join("user_daily");
    let campaign_dir = out.join("campaign_daily");
    std::fs::create_dir_all(&user_dir).expect("creating user_daily/");
    std::fs::create_dir_all(&campaign_dir).expect("creating campaign_daily/");
    let categories = &products.categories;
    std::thread::scope(|s| {
        for (t, acc) in accs.iter().enumerate() {
            if acc.user.is_empty() && t != 0 {
                continue;
            }
            let path = user_dir.join(format!("part-{t:05}.parquet"));
            s.spawn(move || {
                let mut top_values = Vec::new();
                let mut top_levels = Vec::with_capacity(acc.top.len());
                for &c in &acc.top {
                    if c == NO_CATEGORY {
                        top_levels.push(0);
                    } else {
                        top_levels.push(1);
                        top_values.push(ByteArray::from(categories[c as usize].clone()));
                    }
                }
                let columns = vec![
                    Column::Strings(acc.user.iter().map(|&k| ByteArray::from(key::to_bytes(k))).collect()),
                    Column::Int32(acc.day.clone()),
                    Column::Int64(acc.sessions.clone()),
                    Column::Int64(acc.events.clone()),
                    Column::Int64(acc.revenue.clone()),
                    Column::OptionalStrings(top_values, top_levels),
                ];
                output::write(&path, output::USER_SCHEMA, columns).expect("writing user_daily");
            });
        }

        let mut stats = vec![[0i64; 4]; (shape.campaigns + 1) * shape.days];
        for acc in &accs {
            for (total, part) in stats.iter_mut().zip(&acc.stats) {
                for k in 0..4 {
                    total[k] += part[k];
                }
            }
        }
        let (mut ids, mut channels, mut days) = (Vec::new(), Vec::new(), Vec::new());
        let (mut sess, mut purch, mut rev, mut assisted) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for c in 0..=shape.campaigns {
            for d in 0..shape.days {
                let st = stats[c * shape.days + d];
                if st[0] == 0 && st[3] == 0 {
                    continue;
                }
                let (id, channel) = if c == 0 {
                    (b"organic".to_vec(), b"organic".to_vec())
                } else {
                    (campaigns.ids[c - 1].clone(), campaigns.channels[c - 1].clone())
                };
                ids.push(ByteArray::from(id));
                channels.push(ByteArray::from(channel));
                days.push((first_day + d as i64) as i32);
                sess.push(st[0]);
                purch.push(st[1]);
                rev.push(st[2]);
                assisted.push(st[3]);
            }
        }
        let user_sessions: i64 = accs.iter().flat_map(|a| &a.sessions).sum();
        let user_revenue: i64 = accs.iter().flat_map(|a| &a.revenue).sum();
        let camp_sessions: i64 = sess.iter().sum();
        let camp_revenue: i64 = rev.iter().sum();
        if user_sessions != camp_sessions || user_revenue != camp_revenue {
            eprintln!("SELF-CHECK FAILED: sessions {user_sessions} vs {camp_sessions}, revenue {user_revenue} vs {camp_revenue}");
        }
        eprintln!(
            "user_daily rows {}, campaign_daily rows {}, sessions {user_sessions}, revenue {user_revenue}",
            accs.iter().map(|a| a.user.len()).sum::<usize>(),
            ids.len()
        );
        let columns = vec![
            Column::Strings(ids),
            Column::Strings(channels),
            Column::Int32(days),
            Column::Int64(sess),
            Column::Int64(purch),
            Column::Int64(rev),
            Column::Int64(assisted),
        ];
        output::write(&campaign_dir.join("part-00000.parquet"), output::CAMPAIGN_SCHEMA, columns)
            .expect("writing campaign_daily");
    });
    let peak = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).map(|l| l.split_whitespace().skip(1).collect::<Vec<_>>().join(" ")))
        .unwrap_or_default();
    log(&format!("written; peak RSS {peak}"));
    std::process::exit(0);
}
