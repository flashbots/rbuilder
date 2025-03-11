use crate::beacon_api_client::{Client, PayloadAttributesTopic};
use alloy_rpc_types_beacon::events::PayloadAttributesEvent;
use futures::future::join_all;

use tokio::{
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
    time::timeout,
};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Source subscribed to a CL client's payload_attributes event.
/// It makes no attempt to fix problems, it stops sending if any problem arises.
/// It might even fail on the first recv if the connection fails.
/// Usage:
/// let cancellation_token = CancellationToken::new();
/// let source = CLPayloadSource::new("http://127.0.0.1:3500".to_string(), cancellation_token.clone());
/// while let Some(event) = source.recv().await {
/// //Do something with event
/// }
pub struct CLPayloadSource {
    receiver: mpsc::UnboundedReceiver<PayloadAttributesEvent>,
    /// In case we cancel via the CancellationToken this handle allows us to wait for the internal spawned task to end.
    pub join_handle: JoinHandle<()>,
}

impl CLPayloadSource {
    pub fn new(cl: Client, cancellation: CancellationToken) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let join_handle = tokio::spawn(async move {
            if let Ok(mut subscription) = cl.get_events::<PayloadAttributesTopic>().await {
                loop {
                    tokio::select! {
                        _ = cancellation.cancelled() =>{
                            return;
                        }
                        opt_event = subscription.next() => {
                            let event_res = match opt_event {
                                    Some(event_res) => event_res,
                                    None => {
                                        warn!("CL SSE channel closed");
                                        return;
                                    }
                            };
                            match event_res {
                                Ok(event) => {
                                    if sender.send(event).is_err() {
                                        error!("Error while sending payload event,CLPayloadSource closed");
                                        return;
                                    }
                                }
                                Err(err) => {
                                    error!(?err, "Error while receiving CL SEE event, ignoring");
                                }
                            }
                        }
                    }
                }
            }
        });
        Self {
            receiver,
            join_handle,
        }
    }

    async fn recv(&mut self) -> Option<PayloadAttributesEvent> {
        self.receiver.recv().await
    }
}

/// Adds reconnection to a CLPayloadSource.
/// Recreates the PayloadSource if:
/// - PayloadSource::recv returns None
/// - PayloadSource::recv does not deliver a new PayloadAttributesEvent in some time (recv_timeout)
#[derive(Debug)]
pub struct PayloadSourceReconnector {
    receiver: mpsc::UnboundedReceiver<PayloadAttributesEvent>,
    /// In case we cancel via the CancellationToken this handle allows us to wait for the internal spawned task to end.
    pub join_handle: JoinHandle<()>,
}

impl PayloadSourceReconnector {
    /// loop source.recv()+sender.send() handling errors.
    /// Returns true if should continue retrying (!cancellation.is_cancelled())
    async fn poll_payloads(
        source: &mut CLPayloadSource,
        sender: &UnboundedSender<PayloadAttributesEvent>,
        recv_timeout: std::time::Duration,
        cancellation: &CancellationToken,
    ) -> bool {
        loop {
            let timeout_res = timeout(recv_timeout, source.recv());
            tokio::select! {
                _ = cancellation.cancelled() =>{
                    return false;
                }
                res = timeout_res => {
                    match res {
                        Ok(recv_res) => {
                            match recv_res {
                                Some(payload) =>{
                                    if sender.send(payload).is_err() {
                                        error!("Error while sending payload event at PayloadSourceReconnector");
                                    }
                                },
                                None =>{
                                    error!("PayloadSource stopped, reconnecting");
                                    return true;
                                },
                            }
                        }
                        Err(_) => {
                            error!("Too long waiting for a Payload, reconnecting");
                            return true;
                        }
                    }

                }

            }
        }
    }

    /// reconnect_wait is the time it waits before reconnecting to avoid 100% CPU reconnection loop and killing the CL machine.
    pub fn new(
        cl: Client,
        recv_timeout: std::time::Duration,
        reconnect_wait: std::time::Duration,
        cancellation: CancellationToken,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let join_handle = tokio::spawn(async move {
            loop {
                info!("PayloadSourceReconnector connecting");
                let mut source = CLPayloadSource::new(cl.clone(), cancellation.clone());
                if !Self::poll_payloads(&mut source, &sender, recv_timeout, &cancellation).await {
                    return;
                }
                info!("PayloadSourceReconnector waiting to reconnect");
                let timeout_res = timeout(reconnect_wait, cancellation.cancelled()).await;
                if timeout_res.is_ok() {
                    return; // cancelled
                }
            }
        });
        Self {
            receiver,
            join_handle,
        }
    }

    pub async fn recv(&mut self) -> Option<PayloadAttributesEvent> {
        self.receiver.recv().await
    }
}

/// Multiplexes PayloadSourceReconnector to have redundancy in case a CL client dies.
/// It does NOT care about the order of the slots it just notifies the new slots in the received order so it
/// could go "back in time" if we have 2 PayloadSources and one of them is way behind the other.
pub struct PayloadSourceMuxer {
    receiver: mpsc::UnboundedReceiver<PayloadAttributesEvent>,
    /// In case we cancel via the CancellationToken this handle allows us to wait for the internal spawned task to end.
    join_handles: Vec<JoinHandle<()>>,
}

impl PayloadSourceMuxer {
    pub fn new(
        cls: &[Client],
        recv_timeout: std::time::Duration,
        reconnect_wait: std::time::Duration,
        cancellation: CancellationToken,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut join_handles: Vec<JoinHandle<()>> = Vec::new();
        for cl in cls {
            let sender = sender.clone();
            let cancellation = cancellation.clone();
            let cl = cl.clone();
            let join_handle = tokio::spawn(async move {
                let mut source =
                    PayloadSourceReconnector::new(cl, recv_timeout, reconnect_wait, cancellation);
                while let Some(payload) = source.recv().await {
                    if sender.send(payload).is_err() {
                        error!("PayloadSourceMuxer send error");
                    }
                }
            });
            join_handles.push(join_handle);
        }

        Self {
            receiver,
            join_handles,
        }
    }

    /// Waits for all internal tasks to end
    pub async fn join(&mut self) {
        join_all(self.join_handles.drain(..)).await;
    }

    pub async fn recv(&mut self) -> Option<PayloadAttributesEvent> {
        self.receiver.recv().await
    }
}
