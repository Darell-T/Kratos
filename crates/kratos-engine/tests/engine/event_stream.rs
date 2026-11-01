use std::num::NonZeroUsize;
use std::thread;
use std::time::{Duration, Instant};

use kratos_client::bench_post;
use kratos_protocol::{Seq, ThreadEventBody};

use crate::running_engine::{
    RunningEngine, WAIT, expect_silence, next_event, next_seqs, post, subscribed,
};

fn text_of(body: ThreadEventBody) -> String {
    let ThreadEventBody::UserMessage { text } = body;
    text
}

#[test]
fn two_clients_posting_50_messages_each_see_100_contiguous_events() {
    // Catches a seq race between concurrent appends, which would repeat or skip a number, and a
    // follower that drops events under load (lane 10).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let thread = engine.create_thread();

    let (readings, texts) = thread::scope(|scope| {
        let readers = [0, 1].map(|_| {
            scope.spawn(|| {
                let mut socket = subscribed(&engine, &thread.id, 0);
                (0..100)
                    .map(|_| next_event(&mut socket))
                    .collect::<Vec<_>>()
            })
        });
        let posters = ["a", "b"].map(|who| {
            let client = engine.client();
            let thread = &thread;
            scope.spawn(move || {
                let texts: Vec<String> = (1..=50).map(|number| format!("{who}-{number}")).collect();
                texts.iter().for_each(|text| {
                    post(&client, thread, text);
                });
                texts
            })
        });
        let texts: Vec<Vec<String>> = posters
            .into_iter()
            .map(|poster| poster.join().unwrap())
            .collect();
        (readers.map(|reader| reader.join().unwrap()), texts)
    });

    for events in readings {
        let seqs: Vec<u64> = events.iter().map(|event| event.seq.get()).collect();
        assert_eq!(seqs, (1..=100).collect::<Vec<_>>());
        let received: Vec<String> = events
            .into_iter()
            .map(|event| text_of(event.event))
            .collect();
        for sent in &texts {
            let own: Vec<&String> = received.iter().filter(|text| sent.contains(text)).collect();
            assert_eq!(
                own,
                sent.iter().collect::<Vec<_>>(),
                "each client's messages keep their order"
            );
        }
    }
}

#[test]
fn a_stalled_subscriber_delays_neither_posts_nor_other_subscribers_and_catches_up() {
    // Catches an appender that waits on a socket, head-of-line blocking between subscribers, and
    // events lost or repeated after a stalled client reads again. 32 MiB is far more than the
    // socket buffers between the engine and a client that has stopped reading.
    const MESSAGES: u64 = 64;
    const FILLER_BYTES: usize = 512 * 1024;
    // An appender that waited on the stalled socket would hang once the socket buffers fill, and
    // a post would fail on the client's 30 s timeout. So this limit only has to catch a slow
    // wait, and it sits far above the time 32 MiB of synced commits takes on a slow disk, which
    // was over 10 s on GitHub's Windows runner.
    const POSTING_LIMIT: Duration = Duration::from_secs(60);
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let client = engine.client();
    let thread = engine.create_thread();
    let mut stalled = subscribed(&engine, &thread.id, 0);
    let mut active = subscribed(&engine, &thread.id, 0);
    let filler = "x".repeat(FILLER_BYTES);

    let started = Instant::now();
    for number in 1..=MESSAGES {
        post(&client, &thread, &format!("{number}:{filler}"));
    }
    assert!(
        started.elapsed() < POSTING_LIMIT,
        "posting waited on the stalled subscriber"
    );

    for number in 1..=MESSAGES {
        let event = next_event(&mut active);
        assert_eq!(event.seq.get(), number);
    }
    for number in 1..=MESSAGES {
        let event = next_event(&mut stalled);
        assert_eq!(event.seq.get(), number);
        assert!(text_of(event.event).starts_with(&format!("{number}:")));
    }
    expect_silence(&mut stalled);
}

#[test]
fn a_client_resumes_after_its_last_seq_across_an_engine_restart() {
    // Catches events lost across a restart, numbering that restarts after reopening, a follower
    // that cannot reconnect, and a lock left behind by a killed engine (lanes 3 and 4).
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());
    let thread = engine.create_thread();
    let client = engine.client();
    for text in ["one", "two", "three"] {
        post(&client, &thread, text);
    }
    let mut follower = client.follow(thread.id.clone(), Seq::BEFORE_FIRST);
    for number in 1..=3 {
        assert_eq!(follower.next_event(WAIT).unwrap().seq, Seq::new(number));
    }
    let port = engine.port();
    engine.kill();

    let engine = RunningEngine::start_on(home.path(), port);
    for text in ["four", "five"] {
        post(&engine.client(), &thread, text);
    }

    assert_eq!(
        follower.next_event(WAIT).unwrap().seq,
        Seq::new(4),
        "resumed after 3, not from the start"
    );
    assert_eq!(follower.next_event(WAIT).unwrap().seq, Seq::new(5));
    let mut replay = subscribed(&engine, &thread.id, 0);
    assert_eq!(next_seqs(&mut replay, 5), [1, 2, 3, 4, 5]);
    expect_silence(&mut replay);
}

#[test]
fn the_bench_probe_runs_against_the_engine_and_reports_every_message() {
    // Catches a probe that miscounts, hangs, or reports an unordered summary, which would make the
    // perf lane fail or print nonsense.
    let home = tempfile::tempdir().unwrap();
    let engine = RunningEngine::start(home.path());

    let report = bench_post(&engine.client(), NonZeroUsize::new(20).unwrap()).unwrap();

    assert_eq!(report.messages, 20);
    let sorted = [
        report.p50_ms,
        report.p90_ms,
        report.p95_ms,
        report.p99_ms,
        report.max_ms,
    ];
    assert!(
        sorted.windows(2).all(|pair| pair[0] <= pair[1]),
        "{report:?}"
    );
    assert!(report.p50_ms > 0.0, "{report:?}");
}
