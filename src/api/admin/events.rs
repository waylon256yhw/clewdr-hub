use std::{convert::Infallible, time::Duration};

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use tokio::sync::broadcast;

use crate::state::AdminEvent;

pub async fn events(
    State(event_tx): State<broadcast::Sender<AdminEvent>>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let mut rx = event_tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Ok(payload) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(payload));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Ok(payload) = serde_json::to_string(&AdminEvent::request_logs_refresh()) {
                        yield Ok(Event::default().data(payload));
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}
