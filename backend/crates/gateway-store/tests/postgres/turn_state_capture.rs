use std::{sync::Arc, time::Duration};

use chrono::Utc;
use gateway_admin::{
    model::{
        PageSize,
        turn_state::TurnStateSource,
        turn_state_capture::{
            TurnStateProbeExchangeCapture, TurnStateProbeExchangeQuery, TurnStateProbeHeader,
        },
    },
    ports::turn_state_capture::{TurnStateProbeCaptureSink as _, TurnStateProbeCaptureStore as _},
};
use gateway_core::{lifecycle::CancellationToken, task::DaemonTask as _};
use gateway_store::postgres::PgTurnStateProbeCapture;

use super::TestDatabase;

fn header(name: &str, value: &[u8]) -> TurnStateProbeHeader {
    TurnStateProbeHeader {
        name: name.to_owned(),
        value: value.to_vec(),
    }
}

#[tokio::test]
async fn capture_writer_preserves_duplicate_and_binary_headers() {
    let Some(database) = TestDatabase::create("turn_state_capture").await else {
        return;
    };
    let (capture, writer) = PgTurnStateProbeCapture::new(database.pool.clone());
    let capture = Arc::new(capture);
    capture.start(Duration::from_secs(15 * 60));
    let started_at = Utc::now();
    assert!(capture.try_capture(TurnStateProbeExchangeCapture {
        id: "0199f0ec-e797-7d7d-a600-000000000001".to_owned(),
        trigger: TurnStateSource::ManualProbe,
        account_id: "acct_capture".to_owned(),
        model: "gpt-5.4".to_owned(),
        target_id: "proxy-a".to_owned(),
        target_label: "出口 A".to_owned(),
        request_id: "0199f0ec-e797-7d7d-a600-000000000002".to_owned(),
        started_at,
        finished_at: started_at + chrono::Duration::milliseconds(12),
        status_code: Some(200),
        http_version: Some("HTTP/2.0".to_owned()),
        outcome: "state_acquired".to_owned(),
        latency_ms: 12,
        request_headers: vec![header("authorization", b"Bearer test")],
        response_headers: vec![
            header("x-codex-turn-state", &[0xff, 0x00]),
            header("x-codex-turn-state", b"duplicate"),
            header("set-cookie", b"a=b"),
        ],
    }));

    let cancellation = CancellationToken::new();
    let writer = Arc::new(writer);
    let task = tokio::spawn({
        let writer = Arc::clone(&writer);
        let cancellation = cancellation.clone();
        async move { writer.run(cancellation).await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while capture.status().buffer.persisted_total != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("capture writer should persist the exchange");

    let page = capture
        .list(TurnStateProbeExchangeQuery {
            page: 1,
            page_size: PageSize::new(20).expect("page size"),
            account_id: Some("acct_capture".to_owned()),
            model: None,
            trigger: None,
            status_code: None,
            state_only: true,
            start: None,
            end: None,
        })
        .await
        .expect("list capture");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].response_turn_state.count, 2);
    assert!(page.items[0].response_turn_state.present);
    assert_eq!(page.items[0].response_turn_state.byte_length, None);
    assert!(!page.items[0].response_turn_state.valid_292);

    let detail = capture
        .detail("0199f0ec-e797-7d7d-a600-000000000001")
        .await
        .expect("read capture")
        .expect("capture detail");
    assert_eq!(detail.response_headers[0].value, [0xff, 0x00]);
    assert_eq!(detail.response_headers[1].name, "x-codex-turn-state");

    cancellation.cancel();
    task.await
        .expect("capture writer task")
        .expect("capture writer cancellation");
    database.close().await;
}
