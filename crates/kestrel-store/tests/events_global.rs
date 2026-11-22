//! Issue #32, scenario 8: `events_since_global` streams the events of one whole run tree in commit
//! order, with the `events.cursor` column (an explicit `INTEGER PRIMARY KEY` since #98) as the
//! cursor.

// Test helpers outside `#[test]` fns are not covered by `allow-expect-in-tests`; a panic is the
// right way to fail here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use kestrel_store::Store;
use kestrel_types::{AgentSpec, Event, EventKind, RunId, RunStatus};
use tempfile::TempDir;

fn agent(name: &str) -> AgentSpec {
    let yaml = format!(
        "name: {name}\nruntime: opencode\nrole: r\nmodel_policy: {{ default: tier:cheap }}\n\
         sandbox: {{ tier: auto, cpu: 1, memory: 1Gi, workspace: empty-dir }}\n\
         budget: {{ usd: 1.00, wall_clock: 10m }}\n"
    );
    AgentSpec::from_yaml(&yaml).expect("fixture parses")
}

fn msg(text: &str) -> EventKind {
    EventKind::AgentMessage { text: text.into() }
}

fn root(store: &Store, name: &str) -> RunId {
    store.create_run(&agent(name), name).expect("create_run")
}

fn child(store: &Store, parent: RunId, name: &str) -> RunId {
    store
        .create_child_run(parent, &agent(name), name)
        .expect("create_child_run")
}

fn global(store: &Store, root: RunId, after: u64) -> Vec<(u64, Event)> {
    store
        .events_since_global(root, after)
        .expect("events_since_global")
}

fn cursors(stream: &[(u64, Event)]) -> Vec<u64> {
    stream.iter().map(|(cursor, _)| *cursor).collect()
}

fn events(stream: &[(u64, Event)]) -> Vec<Event> {
    stream.iter().map(|(_, event)| event.clone()).collect()
}

/// `(cursor, run_id, seq)` of every row in `events`, in cursor order, read with a raw connection.
fn raw_events(path: &Path) -> Vec<(u64, RunId, u64)> {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let mut stmt = conn
        .prepare("SELECT cursor, run_id, seq FROM events ORDER BY cursor")
        .expect("prepare");
    let raw = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("rows");
    raw.into_iter()
        .map(|(cursor, run, seq)| {
            (
                u64::try_from(cursor).expect("cursor is positive"),
                run.parse().expect("run id parses"),
                u64::try_from(seq).expect("seq is positive"),
            )
        })
        .collect()
}

/// Two trees whose events were committed interleaved.
///
/// Tree A: `root_a`, its child `a_child`, and `a_grand` below that. Tree B: `root_b` and
/// `b_child`. `a_events` and `b_events` hold each tree's events in the order they were committed.
struct Forest {
    store: Store,
    path: PathBuf,
    root_a: RunId,
    a_child: RunId,
    a_grand: RunId,
    root_b: RunId,
    b_child: RunId,
    a_events: Vec<Event>,
    b_events: Vec<Event>,
}

impl Forest {
    fn tree_a(&self) -> HashSet<RunId> {
        HashSet::from([self.root_a, self.a_child, self.a_grand])
    }
}

fn forest(dir: &TempDir) -> Forest {
    let path = dir.path().join("kestrel.db");
    let store = Store::open(&path).expect("open");
    let root_a = root(&store, "a-root");
    let root_b = root(&store, "b-root");
    let a_child = child(&store, root_a, "a-child");
    let b_child = child(&store, root_b, "b-child");
    let a_grand = child(&store, a_child, "a-grand");

    let mut a_events = Vec::new();
    let mut b_events = Vec::new();
    // (run, is it in tree A), in commit order.
    let order = [
        (root_a, true),
        (root_b, false),
        (a_child, true),
        (b_child, false),
        (a_grand, true),
        (root_b, false),
        (root_b, false),
        (a_child, true),
        (root_a, true),
        (b_child, false),
        (a_grand, true),
        (a_grand, true),
    ];
    for (i, (run, in_a)) in order.into_iter().enumerate() {
        let event = store.append(run, msg(&format!("m{i}"))).expect("append");
        if in_a {
            a_events.push(event);
        } else {
            b_events.push(event);
        }
    }

    // A status change writes an event too. It sits in the stream at its commit position.
    for (run, log) in [(a_child, &mut a_events), (b_child, &mut b_events)] {
        store
            .transition(run, &RunStatus::Queued, RunStatus::Provisioning)
            .expect("transition");
        let changed = store
            .events_since(run, 0)
            .expect("events_since")
            .pop()
            .expect("the StatusChanged event");
        assert_eq!(
            changed.kind,
            EventKind::StatusChanged {
                from: RunStatus::Queued,
                to: RunStatus::Provisioning,
            }
        );
        log.push(changed);
    }
    b_events.push(store.append(root_b, msg("b-last")).expect("append"));
    a_events.push(store.append(root_a, msg("a-last")).expect("append"));

    Forest {
        store,
        path,
        root_a,
        a_child,
        a_grand,
        root_b,
        b_child,
        a_events,
        b_events,
    }
}

// 8. Given two roots with interleaved events, when `events_since_global(root_a, cursor)` is read,
//    then only tree A events come back, in commit order, and a second read from the last cursor
//    returns nothing.
#[test]
fn s8_global_read_returns_only_tree_a_in_commit_order_then_nothing_from_the_last_cursor() {
    let dir = TempDir::new().expect("tempdir");
    let f = forest(&dir);

    let stream = global(&f.store, f.root_a, 0);
    assert_eq!(
        events(&stream),
        f.a_events,
        "tree A events, in commit order"
    );
    assert_eq!(stream.len(), 9);
    let tree_a = f.tree_a();
    for (_, event) in &stream {
        assert!(
            tree_a.contains(&event.run_id),
            "event of a run outside tree A: {event:?}"
        );
    }
    // All three runs of the tree are in the stream, the root's own events included.
    let seen: HashSet<RunId> = stream.iter().map(|(_, e)| e.run_id).collect();
    assert_eq!(seen, tree_a);

    let a_cursors = cursors(&stream);
    assert!(
        a_cursors.windows(2).all(|w| w[0] < w[1]),
        "cursors must strictly increase: {a_cursors:?}"
    );
    assert!(
        a_cursors[0] > 0,
        "0 means read from the start: {a_cursors:?}"
    );

    let last = *a_cursors.last().expect("tree A has events");
    assert!(
        global(&f.store, f.root_a, last).is_empty(),
        "a read from the last cursor returns nothing"
    );

    // Tree B is its own stream.
    let stream_b = global(&f.store, f.root_b, 0);
    assert_eq!(events(&stream_b), f.b_events);
    assert_eq!(stream_b.len(), 7);
    let both: HashSet<u64> = a_cursors.into_iter().chain(cursors(&stream_b)).collect();
    assert_eq!(both.len(), 16, "every event has its own cursor");
}

// 8. "... using the `events` rowid as the cursor", which #98 made the explicit `events.cursor`
//    column: the stream hands out the `cursor` values of the tree's rows.
#[test]
fn s8_the_cursor_is_the_events_cursor_column() {
    let dir = TempDir::new().expect("tempdir");
    let f = forest(&dir);
    let tree_a = f.tree_a();

    let want: Vec<(u64, RunId, u64)> = raw_events(&f.path)
        .into_iter()
        .filter(|(_, run, _)| tree_a.contains(run))
        .collect();
    let got: Vec<(u64, RunId, u64)> = global(&f.store, f.root_a, 0)
        .into_iter()
        .map(|(cursor, event)| (cursor, event.run_id, event.seq))
        .collect();
    assert_eq!(got, want);

    // Tree B rows sit between tree A rows, so tree A's cursors have gaps.
    let cursors: Vec<u64> = got.iter().map(|(cursor, ..)| *cursor).collect();
    assert!(
        cursors.windows(2).any(|w| w[1] - w[0] > 1),
        "the fixture must interleave the trees: {cursors:?}"
    );
}

// 8. Reading from any cursor returns exactly the tree's later events.
#[test]
fn s8_reading_from_a_cursor_returns_only_later_events() {
    let dir = TempDir::new().expect("tempdir");
    let f = forest(&dir);
    let all = global(&f.store, f.root_a, 0);

    for (i, (cursor, _)) in all.iter().enumerate() {
        assert_eq!(
            global(&f.store, f.root_a, *cursor),
            all[i + 1..],
            "read after cursor {cursor}"
        );
    }

    // A cursor taken from tree B is still a position in the same log.
    let b_cursor = global(&f.store, f.root_b, 0)[2].0;
    let later: Vec<(u64, Event)> = all
        .iter()
        .filter(|(cursor, _)| *cursor > b_cursor)
        .cloned()
        .collect();
    assert!(!later.is_empty() && later.len() < all.len());
    assert_eq!(global(&f.store, f.root_a, b_cursor), later);

    // A cursor past the end, even one no event can have, is an empty read and not an error.
    let last = all.last().expect("tree A has events").0;
    assert!(global(&f.store, f.root_a, last + 1_000).is_empty());
    assert!(global(&f.store, f.root_a, u64::MAX).is_empty());
}

// 8. A reader that keeps its last cursor gets each new tree event exactly once, also from runs
//    that joined the tree later, and from a second handle on the same file.
#[test]
fn s8_new_tree_events_arrive_after_the_last_cursor() {
    let dir = TempDir::new().expect("tempdir");
    let f = forest(&dir);
    let reader = Store::open(&f.path).expect("second handle");
    let start = *cursors(&global(&reader, f.root_a, 0))
        .last()
        .expect("tree A has events");
    let mut cursor = start;

    // An event in tree B is not news for tree A.
    f.store.append(f.b_child, msg("b-more")).expect("append");
    assert!(global(&reader, f.root_a, cursor).is_empty());

    // An event deep in tree A is.
    let deep = f.store.append(f.a_grand, msg("a-more")).expect("append");
    let news = global(&reader, f.root_a, cursor);
    assert_eq!(events(&news), vec![deep.clone()]);
    assert!(news[0].0 > cursor);
    cursor = news[0].0;
    assert!(global(&reader, f.root_a, cursor).is_empty());

    // A run created after the reader started is in the stream as soon as it has events.
    let late = child(&f.store, f.a_grand, "a-late");
    let first = f.store.append(late, msg("late-1")).expect("append");
    f.store.append(f.root_b, msg("b-again")).expect("append");
    let second = f.store.append(late, msg("late-2")).expect("append");
    let news = global(&reader, f.root_a, cursor);
    assert_eq!(events(&news), vec![first.clone(), second.clone()]);
    assert!(news[0].0 > cursor && news[1].0 > news[0].0);

    // A reader that never moved past `start` gets all three, and the whole stream is the old
    // one plus those three.
    assert_eq!(
        events(&global(&reader, f.root_a, start)),
        vec![deep.clone(), first.clone(), second.clone()]
    );
    let mut want = f.a_events.clone();
    want.extend([deep, first, second]);
    assert_eq!(events(&global(&f.store, f.root_a, 0)), want);
}

// 8. A root without children is a tree of one: its global stream is its own events.
#[test]
fn s8_a_lone_root_streams_its_own_events_and_an_unknown_root_streams_nothing() {
    let store = Store::open_in_memory().expect("open");
    let lone = root(&store, "lone");
    let other = root(&store, "other");
    assert!(global(&store, lone, 0).is_empty(), "no events yet");

    store.append(lone, msg("one")).expect("append");
    store.append(other, msg("noise")).expect("append");
    store
        .transition(lone, &RunStatus::Queued, RunStatus::Cancelled)
        .expect("transition");

    let stream = global(&store, lone, 0);
    assert_eq!(
        events(&stream),
        store.events_since(lone, 0).expect("events_since")
    );
    assert_eq!(stream.len(), 2);

    // Like `events_since` for an unknown run: nothing, and not an error.
    assert!(global(&store, RunId::new(), 0).is_empty());
}
