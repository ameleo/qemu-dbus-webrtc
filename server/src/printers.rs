// SPDX-License-Identifier: MPL-2.0

//! Client printers, shown in the guest as Windows printers through the guest agent.
//!
//! A viewer announces its printers, with their driver's capabilities and default settings; the
//! agent makes each a printer of the guest. When a guest application prints, the job comes to
//! the server (`fr.ameleo.Host.Printer1`), which sends it to the viewer that owns the printer,
//! as it comes, a window of bytes ahead of the viewer's acknowledgements. The driver's own
//! settings dialog opens on the viewer too. When the viewer goes, its printers go too; when the
//! agent comes back (the guest restarted), the printers are added again.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use qdw_protocol::PRINTER_WINDOW;
use qdw_protocol::printer::{Reassembler, encode};
use qdw_protocol::v1::{
    PrinterAdded, PrinterJob, PrinterJobAbort, PrinterJobData, PrinterJobEnd, PrinterMessage,
    PrinterProperties, printer_message::Message,
};
use qdw_qemu::agent::{self, PrinterProxy, printer};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// How long the guest waits for the viewer to take a piece of a job.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the guest waits for the viewer to have handed a whole job to its printer.
const END_TIMEOUT: Duration = Duration::from_secs(300);

/// A guest job call, for the viewer that owns the printer.
pub enum PrinterCall {
    Begin(PrinterJob),
    /// Answered when the viewer may take more.
    Write {
        job_id: u32,
        data: Vec<u8>,
        taken: oneshot::Sender<()>,
    },
    /// Answered when the viewer has the whole job, or could not print it.
    End {
        job_id: u32,
        done: oneshot::Sender<std::result::Result<(), String>>,
    },
    Abort {
        job_id: u32,
    },
    Properties(PrinterProperties),
}

/// A printer shown in the guest.
#[derive(Clone)]
struct Printer {
    viewer: u64,
    client_id: u32,
    name: String,
    is_default: bool,
    capabilities: String,
    default_ticket: String,
    calls: mpsc::UnboundedSender<PrinterCall>,
    /// To tell the viewer what became of its printer, again when the agent comes back.
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
}

/// A job of the guest, being sent to a viewer.
struct Job {
    /// The server's id of the job, which the viewer knows.
    id: u32,
    calls: mpsc::UnboundedSender<PrinterCall>,
}

/// The printers of all viewers.
pub struct Printers {
    agent: Mutex<Option<PrinterProxy<'static>>>,
    /// By the guest's id of the printer.
    printers: Mutex<HashMap<u32, Printer>>,
    /// By the guest's id of the job.
    jobs: Mutex<HashMap<u32, Job>>,
    next_id: AtomicU32,
    next_job: AtomicU32,
    viewers: AtomicU64,
}

fn added(client_id: u32, error: String) -> Vec<Vec<u8>> {
    encode(&PrinterMessage {
        message: Some(Message::Added(PrinterAdded {
            id: client_id,
            error,
        })),
    })
}

fn send(outgoing: &mpsc::UnboundedSender<Vec<u8>>, messages: Vec<Vec<u8>>) {
    for message in messages {
        let _ = outgoing.send(message);
    }
}

async fn add(
    agent: &PrinterProxy<'static>,
    id: u32,
    p: &Printer,
) -> std::result::Result<(), String> {
    agent
        .add_printer(
            id,
            &p.name,
            p.is_default,
            &p.capabilities,
            &p.default_ticket,
        )
        .await
        .map_err(|e| e.to_string())
}

impl Printers {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            agent: Mutex::new(None),
            printers: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            next_job: AtomicU32::new(1),
            viewers: AtomicU64::new(0),
        })
    }

    fn agent(&self) -> Option<PrinterProxy<'static>> {
        self.agent.lock().unwrap().clone()
    }

    /// Add the interface the guest calls for its jobs to an agent connection.
    pub fn serve(
        self: &Arc<Self>,
        builder: zbus::connection::Builder<'static>,
    ) -> zbus::Result<zbus::connection::Builder<'static>> {
        builder.serve_at(
            agent::HOST_PATH,
            HostPrinter {
                printers: self.clone(),
            },
        )
    }

    /// The agent is connected: add the printers the viewers announced.
    pub async fn attach(&self, connection: &zbus::Connection) -> Result<()> {
        let proxy = PrinterProxy::new(connection).await?;
        *self.agent.lock().unwrap() = Some(proxy.clone());
        let printers: Vec<(u32, Printer)> = self
            .printers
            .lock()
            .unwrap()
            .iter()
            .map(|(&id, p)| (id, p.clone()))
            .collect();
        for (id, p) in printers {
            let error = add(&proxy, id, &p).await.err().unwrap_or_default();
            send(&p.outgoing, added(p.client_id, error));
        }
        Ok(())
    }

    /// The agent went away, its jobs with it.
    pub fn detach(&self) {
        *self.agent.lock().unwrap() = None;
        let jobs: Vec<Job> = self.jobs.lock().unwrap().drain().map(|(_, j)| j).collect();
        for job in jobs {
            let _ = job.calls.send(PrinterCall::Abort { job_id: job.id });
        }
    }

    /// A viewer's side of the printers, with what its loop follows.
    pub fn viewer(self: &Arc<Self>) -> (ViewerPrinters, PrintersFollow) {
        let (calls_tx, calls) = mpsc::unbounded_channel();
        let (outgoing_tx, outgoing) = mpsc::unbounded_channel();
        let viewer = ViewerPrinters {
            printers: self.clone(),
            id: self.viewers.fetch_add(1, Ordering::Relaxed),
            calls_tx,
            outgoing: outgoing_tx,
            jobs: HashMap::new(),
            properties: HashSet::new(),
            reassembler: Reassembler::default(),
        };
        (viewer, PrintersFollow { calls, outgoing })
    }
}

/// What a viewer's loop follows for the printers.
pub struct PrintersFollow {
    /// Job calls for this viewer's printers.
    pub calls: mpsc::UnboundedReceiver<PrinterCall>,
    /// Messages to send to the client, from work done apart.
    pub outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// A job being sent to the client.
#[derive(Default)]
struct ViewerJob {
    /// Bytes sent that the client did not acknowledge yet.
    unacknowledged: u64,
    /// The guest's piece waiting for room in the window.
    taken: Option<oneshot::Sender<()>>,
    /// The guest waiting for the client to have the whole job.
    done: Option<oneshot::Sender<std::result::Result<(), String>>>,
}

/// One viewer's side of the printers.
pub struct ViewerPrinters {
    printers: Arc<Printers>,
    id: u64,
    calls_tx: mpsc::UnboundedSender<PrinterCall>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    /// By the server's id of the job.
    jobs: HashMap<u32, ViewerJob>,
    /// The guest's ids of the settings dialogs open on the client.
    properties: HashSet<u32>,
    reassembler: Reassembler,
}

fn message(message: Message) -> Vec<Vec<u8>> {
    encode(&PrinterMessage {
        message: Some(message),
    })
}

impl ViewerPrinters {
    /// A job call of the guest on one of this viewer's printers: send it to the client.
    pub fn call(&mut self, call: PrinterCall) -> Vec<Vec<u8>> {
        match call {
            PrinterCall::Begin(job) => {
                self.jobs.insert(job.job_id, ViewerJob::default());
                message(Message::Job(job))
            }
            PrinterCall::Write {
                job_id,
                data,
                taken,
            } => {
                let Some(job) = self.jobs.get_mut(&job_id) else {
                    return Vec::new();
                };
                job.unacknowledged += data.len() as u64;
                if job.unacknowledged <= u64::from(PRINTER_WINDOW) {
                    let _ = taken.send(());
                } else {
                    job.taken = Some(taken);
                }
                message(Message::JobData(PrinterJobData { job_id, data }))
            }
            PrinterCall::End { job_id, done } => {
                let Some(job) = self.jobs.get_mut(&job_id) else {
                    let _ = done.send(Err("the job was dropped".into()));
                    return Vec::new();
                };
                job.done = Some(done);
                message(Message::JobEnd(PrinterJobEnd { job_id }))
            }
            PrinterCall::Abort { job_id } => {
                // Whoever waits on the job gets an error.
                self.jobs.remove(&job_id);
                message(Message::JobAbort(PrinterJobAbort { job_id }))
            }
            PrinterCall::Properties(properties) => {
                self.properties.insert(properties.request_id);
                message(Message::Properties(properties))
            }
        }
    }

    /// The guest's id of this viewer's printer `client_id`.
    fn guest_id(&self, client_id: u32) -> Option<u32> {
        let printers = self.printers.printers.lock().unwrap();
        printers
            .iter()
            .find(|(_, p)| p.viewer == self.id && p.client_id == client_id)
            .map(|(&id, _)| id)
    }

    /// A data channel message from the client.
    pub async fn message(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(message) = self.reassembler.push(bytes)? else {
            return Ok(());
        };
        match message.message {
            Some(Message::Announce(a)) => {
                self.announce(a.id, a.name, a.is_default, a.capabilities, a.default_ticket)
            }
            Some(Message::Remove(remove)) => {
                if let Some(id) = self.guest_id(remove.id) {
                    self.remove(id).await;
                }
            }
            Some(Message::JobAck(ack)) => {
                if let Some(job) = self.jobs.get_mut(&ack.job_id) {
                    job.unacknowledged = job.unacknowledged.saturating_sub(u64::from(ack.bytes));
                    if job.unacknowledged <= u64::from(PRINTER_WINDOW)
                        && let Some(taken) = job.taken.take()
                    {
                        let _ = taken.send(());
                    }
                }
            }
            Some(Message::JobDone(done)) => {
                if let Some(job) = self.jobs.remove(&done.job_id) {
                    let result = if done.error.is_empty() {
                        Ok(())
                    } else {
                        Err(done.error)
                    };
                    if let Some(waiting) = job.done {
                        let _ = waiting.send(result);
                    }
                }
            }
            Some(Message::PropertiesDone(done)) => {
                // Only for a dialog the guest asked for.
                if self.properties.remove(&done.request_id)
                    && let Some(agent) = self.printers.agent()
                {
                    agent
                        .document_properties_done(
                            done.request_id,
                            &done.print_ticket,
                            done.cancelled,
                        )
                        .await?;
                }
            }
            // Server to client only, or empty.
            Some(
                Message::Added(_)
                | Message::Job(_)
                | Message::JobData(_)
                | Message::JobEnd(_)
                | Message::JobAbort(_)
                | Message::Properties(_)
                | Message::Fragment(_),
            )
            | None => {}
        }
        Ok(())
    }

    fn announce(
        &mut self,
        client_id: u32,
        name: String,
        is_default: bool,
        capabilities: String,
        default_ticket: String,
    ) {
        let full = self.printers.printers.lock().unwrap().len() >= printer::MAX_PRINTERS;
        let why = if full {
            Some("too many printers")
        } else if self.guest_id(client_id).is_some() {
            Some("this printer id is announced already")
        } else if capabilities.len() > printer::MAX_XML || default_ticket.len() > printer::MAX_XML {
            Some("capabilities or ticket too large")
        } else {
            None
        };
        if let Some(why) = why {
            send(&self.outgoing, added(client_id, why.into()));
            return;
        }
        let id = self.printers.next_id.fetch_add(1, Ordering::Relaxed);
        let printer = Printer {
            viewer: self.id,
            client_id,
            name,
            is_default,
            capabilities,
            default_ticket,
            calls: self.calls_tx.clone(),
            outgoing: self.outgoing.clone(),
        };
        self.printers
            .printers
            .lock()
            .unwrap()
            .insert(id, printer.clone());
        let agent = self.printers.agent();
        // Apart from the viewer's loop: the call carries up to megabytes of XML.
        tokio::spawn(async move {
            let result = match agent {
                Some(agent) => add(&agent, id, &printer).await,
                None => Err("no guest agent: the printer is added when it comes".into()),
            };
            let (name, default) = (&printer.name, printer.is_default);
            match &result {
                Ok(()) => info!(id, name, default, "client printer added to the guest"),
                Err(e) => warn!(id, name, error = e, "client printer not added"),
            }
            send(
                &printer.outgoing,
                added(client_id, result.err().unwrap_or_default()),
            );
        });
    }

    async fn remove(&mut self, id: u32) {
        self.printers.printers.lock().unwrap().remove(&id);
        if let Some(agent) = self.printers.agent()
            && let Err(e) = agent.remove_printer(id).await
        {
            debug!(id, error = %e, "client printer not removed from the guest");
        }
    }

    /// The viewer went away: its printers leave the guest, its jobs fail, its dialogs are
    /// cancelled.
    pub async fn release(&mut self) {
        let ids: Vec<u32> = {
            let printers = self.printers.printers.lock().unwrap();
            printers
                .iter()
                .filter(|(_, p)| p.viewer == self.id)
                .map(|(&id, _)| id)
                .collect()
        };
        for id in ids {
            self.remove(id).await;
        }
        self.jobs.clear();
        if let Some(agent) = self.printers.agent() {
            for request_id in self.properties.drain() {
                let _ = agent.document_properties_done(request_id, "", true).await;
            }
        }
    }
}

/// `fr.ameleo.Host.Printer1`: the guest's jobs on the client printers. A D-Bus error means the
/// client could not take the job.
struct HostPrinter {
    printers: Arc<Printers>,
}

fn failed(why: &str) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(why.into())
}

impl HostPrinter {
    /// The viewer and its id of the guest's printer `printer_id`.
    fn printer(
        &self,
        printer_id: u32,
    ) -> zbus::fdo::Result<(mpsc::UnboundedSender<PrinterCall>, u32)> {
        let printers = self.printers.printers.lock().unwrap();
        let p = printers
            .get(&printer_id)
            .ok_or_else(|| failed("no such client printer"))?;
        Ok((p.calls.clone(), p.client_id))
    }

    /// The viewer and the server's id of the guest's job `job_id`.
    fn job(&self, job_id: u32) -> zbus::fdo::Result<(mpsc::UnboundedSender<PrinterCall>, u32)> {
        let jobs = self.printers.jobs.lock().unwrap();
        let job = jobs.get(&job_id).ok_or_else(|| failed("no such job"))?;
        Ok((job.calls.clone(), job.id))
    }
}

fn gone() -> zbus::fdo::Error {
    failed("the client of this printer is gone")
}

#[zbus::interface(name = "fr.ameleo.Host.Printer1")]
impl HostPrinter {
    async fn begin_job(
        &self,
        printer_id: u32,
        job_id: u32,
        document_format: &str,
        job_name: &str,
        user_name: &str,
        print_ticket: &str,
    ) -> zbus::fdo::Result<()> {
        let (calls, client_printer) = self.printer(printer_id)?;
        let id = self.printers.next_job.fetch_add(1, Ordering::Relaxed);
        self.printers.jobs.lock().unwrap().insert(
            job_id,
            Job {
                id,
                calls: calls.clone(),
            },
        );
        info!(
            printer = printer_id,
            job = id,
            document_format,
            "guest print job"
        );
        calls
            .send(PrinterCall::Begin(PrinterJob {
                job_id: id,
                printer_id: client_printer,
                document_format: document_format.into(),
                job_name: job_name.into(),
                user_name: user_name.into(),
                print_ticket: print_ticket.into(),
            }))
            .map_err(|_| gone())
    }

    async fn write_job(&self, job_id: u32, data: Vec<u8>) -> zbus::fdo::Result<()> {
        let (calls, id) = self.job(job_id)?;
        let (taken, wait) = oneshot::channel();
        calls
            .send(PrinterCall::Write {
                job_id: id,
                data,
                taken,
            })
            .map_err(|_| gone())?;
        match tokio::time::timeout(WRITE_TIMEOUT, wait).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(failed("the job was dropped")),
            Err(_) => Err(failed("the client did not take the job in time")),
        }
    }

    async fn end_job(&self, job_id: u32) -> zbus::fdo::Result<()> {
        let (calls, id) = self.job(job_id)?;
        let (done, wait) = oneshot::channel();
        calls
            .send(PrinterCall::End { job_id: id, done })
            .map_err(|_| gone())?;
        let result = tokio::time::timeout(END_TIMEOUT, wait).await;
        self.printers.jobs.lock().unwrap().remove(&job_id);
        match result {
            Ok(Ok(Ok(()))) => {
                info!(job = id, "print job handed to the client's printer");
                Ok(())
            }
            Ok(Ok(Err(e))) => {
                warn!(job = id, error = e, "the client could not print the job");
                Err(failed(&e))
            }
            Ok(Err(_)) => Err(failed("the job was dropped")),
            Err(_) => Err(failed("the client did not finish the job in time")),
        }
    }

    async fn abort_job(&self, job_id: u32) -> zbus::fdo::Result<()> {
        if let Some(job) = self.printers.jobs.lock().unwrap().remove(&job_id) {
            info!(job = job.id, "guest print job cancelled");
            let _ = job.calls.send(PrinterCall::Abort { job_id: job.id });
        }
        Ok(())
    }

    async fn document_properties(
        &self,
        printer_id: u32,
        request_id: u32,
        print_ticket: &str,
    ) -> zbus::fdo::Result<()> {
        let (calls, client_printer) = self.printer(printer_id)?;
        calls
            .send(PrinterCall::Properties(PrinterProperties {
                request_id,
                printer_id: client_printer,
                print_ticket: print_ticket.into(),
            }))
            .map_err(|_| gone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qdw_protocol::Message as _;
    use qdw_protocol::v1::{PrinterJobAck, PrinterJobDone};

    fn from_client(message: Message) -> Vec<u8> {
        PrinterMessage {
            message: Some(message),
        }
        .encode_to_vec()
    }

    #[tokio::test]
    async fn a_window_of_bytes_ahead_of_the_client() {
        let (mut viewer, _follow) = Printers::new().viewer();
        viewer.call(PrinterCall::Begin(PrinterJob {
            job_id: 1,
            ..Default::default()
        }));
        let piece = 256 * 1024;
        let mut waiting = Vec::new();
        for _ in 0..5 {
            let (taken, wait) = oneshot::channel();
            let sent = viewer.call(PrinterCall::Write {
                job_id: 1,
                data: vec![0; piece],
                taken,
            });
            assert_eq!(sent.len(), 5);
            waiting.push(wait);
        }
        // 1 MiB may wait for the client; the fifth piece waits for room.
        for wait in &mut waiting[..4] {
            assert_eq!(wait.try_recv(), Ok(()));
        }
        assert!(waiting[4].try_recv().is_err());
        let ack = Message::JobAck(PrinterJobAck {
            job_id: 1,
            bytes: piece as u32,
        });
        viewer.message(&from_client(ack)).await.unwrap();
        assert_eq!(waiting[4].try_recv(), Ok(()));

        let (done, mut finished) = oneshot::channel();
        viewer.call(PrinterCall::End { job_id: 1, done });
        assert!(finished.try_recv().is_err());
        let done = Message::JobDone(PrinterJobDone {
            job_id: 1,
            error: "out of paper".into(),
        });
        viewer.message(&from_client(done)).await.unwrap();
        assert_eq!(finished.try_recv(), Ok(Err("out of paper".into())));
    }

    #[tokio::test]
    async fn an_aborted_job_fails_its_waiters() {
        let (mut viewer, _follow) = Printers::new().viewer();
        viewer.call(PrinterCall::Begin(PrinterJob {
            job_id: 3,
            ..Default::default()
        }));
        let (taken, mut wait) = oneshot::channel();
        viewer.call(PrinterCall::Write {
            job_id: 3,
            data: vec![0; 2 * 1024 * 1024],
            taken,
        });
        assert!(wait.try_recv().is_err());
        viewer.call(PrinterCall::Abort { job_id: 3 });
        assert!(matches!(
            wait.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }
}
