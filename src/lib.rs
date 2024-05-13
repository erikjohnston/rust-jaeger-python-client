use crossbeam_channel::{bounded, Sender};
use itertools::Itertools;
use ordered_float::OrderedFloat;
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedStr;
use pyo3::types::{PyDict, PyString};
use pyo3::PyDowncastError;
use thrift::protocol::{TCompactInputProtocol, TCompactOutputProtocol};
use try_from::TryFrom;

use std::io::{self, Write};
use std::mem;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
use std::{
    io::empty,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    usize,
};

mod thrift_gen;

use crate::thrift_gen::agent::TAgentSyncClient;

static CARGO_VERSION: &str = env!("CARGO_PKG_VERSION");

#[pymodule]
/// The root Python module.
fn rust_python_jaeger_reporter(_py: Python, m: Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Reporter>()?;
    m.add_class::<Stats>()?;

    m.add("__version__", CARGO_VERSION)?;

    Ok(())
}

#[pyclass]
#[derive(Debug)]
struct Stats {
    #[pyo3(get)]
    queue_size: usize,
    #[pyo3(get)]
    sent_batches: usize,
    #[pyo3(get)]
    sent_batches_errors: usize,
    #[pyo3(get)]
    span_sender_size: usize,
    #[pyo3(get)]
    process_sender_size: usize,
    #[pyo3(get)]
    last_error: Option<String>,
}

#[pymethods]
impl Stats {
    fn __str__(&self) -> String {
        format!("{:?}", self)
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self)
    }
}

/// The main reporter class.
#[pyclass]
struct Reporter {
    span_sender: Sender<thrift_gen::jaeger::Span>,
    process_sender: Sender<thrift_gen::jaeger::Process>,
    queue_size: Arc<AtomicUsize>,
    sent_batches: Arc<AtomicUsize>,
    sent_batches_errors: Arc<AtomicUsize>,
    last_error: Arc<Mutex<Option<String>>>,
}

#[pymethods]
impl Reporter {
    #[new]
    fn new(config: Option<&Bound<'_, PyDict>>) -> PyResult<Reporter> {
        let mut agent_host_name: String = "127.0.0.1".to_string();
        let mut agent_port: i32 = 6831;

        if let Some(config) = config {
            if let Some(agent_host_name_arg) = config.get_item("agent_host_name")? {
                agent_host_name = agent_host_name_arg.extract().map_err(|_| {
                    pyo3::exceptions::PyTypeError::new_err("'agent_host_name' must be an string")
                })?;
            }

            if let Some(agent_port_arg) = config.get_item("agent_port")? {
                agent_port = agent_port_arg.extract().map_err(|_| {
                    pyo3::exceptions::PyTypeError::new_err("'agent_port' must be an int")
                })?;
            }
        }
        // Set up the UDP transport
        let socket = UdpSocket::bind(
            &(49152..65535)
                .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
                .collect::<Vec<_>>()[..],
        )?;
        socket.connect(format!("{}:{}", agent_host_name, agent_port))?;

        // We never read anything so this can be a no-op input protocol
        let input_protocol = TCompactInputProtocol::new(empty());
        let output_protocol =
            TCompactOutputProtocol::new(TBufferedTransport::new(ConnectedUdp { socket }));
        let mut agent = Box::new(thrift_gen::agent::AgentSyncClient::new(
            input_protocol,
            output_protocol,
        ));

        // We want to do the actual sending in a separate thread. We add bounds
        // here to ensure we don't stack these up infinitely if something goes
        // wrong.
        let (span_sender, span_receiver) = bounded::<thrift_gen::jaeger::Span>(1000);
        let (process_sender, process_receiver) = bounded::<thrift_gen::jaeger::Process>(1000);

        let queue_size = Arc::new(AtomicUsize::new(0));
        let sent_batches = Arc::new(AtomicUsize::new(0));
        let sent_batches_errors = Arc::new(AtomicUsize::new(0));
        let last_error = Arc::new(Mutex::new(None));

        let queue_size_thread = queue_size.clone();
        let sent_batches_thread = sent_batches.clone();
        let sent_batches_errors_thread = sent_batches_errors.clone();
        let last_error_thread = last_error.clone();

        std::thread::Builder::new()
            .name("jaeger_sender".to_string())
            .spawn(move || {
                let mut queue = Vec::<thrift_gen::jaeger::Span>::with_capacity(100);
                let mut process = None;

                let mut last_push = Instant::now();

                loop {
                    // Wait for new span to be queud.
                    if let Ok(span) = span_receiver.recv_timeout(Duration::from_secs(5)) {
                        queue.push(span);
                    }

                    queue_size_thread.store(queue.len(), Ordering::Relaxed);

                    // Check if we have been given any new process information
                    // since the last loop.
                    while let Ok(new_process) = process_receiver.try_recv() {
                        process = Some(new_process);
                    }

                    // We batch up the spans before sending them, waiting at
                    // most N seconds between sends
                    if queue.len() >= 20
                        || (!queue.is_empty() && last_push.elapsed().as_secs() > 20)
                    {
                        last_push = Instant::now();
                        let to_send = mem::replace(
                            &mut queue,
                            Vec::<thrift_gen::jaeger::Span>::with_capacity(100),
                        );

                        if let Some(process) = process.clone() {
                            for chunk in to_send.into_iter().chunks(20).into_iter() {
                                let result = agent.emit_batch(thrift_gen::jaeger::Batch::new(
                                    process.clone(),
                                    chunk.collect(),
                                ));

                                sent_batches_thread.fetch_add(1, Ordering::Relaxed);

                                if let Err(err) = result {
                                    sent_batches_errors_thread.fetch_add(1, Ordering::Relaxed);
                                    *last_error_thread.lock().expect("poisoned") =
                                        Some(err.to_string());
                                }
                            }
                        }
                    }
                }
            })
            .unwrap();

        Ok(Reporter {
            span_sender,
            process_sender,
            queue_size,
            sent_batches,
            sent_batches_errors,
            last_error,
        })
    }

    /// Sets the process information needed to report spans.
    fn set_process(
        self_: PyRef<Self>,
        service_name: String,
        tags: &Bound<'_, PyDict>,
        #[allow(unused_variables)] // Python expects this to exist.
        max_length: i32,
    ) -> PyResult<()> {
        let tags = make_tags(self_.py(), tags)?;

        // This may fail if the queue is full. We should probably log something
        // somehow?
        self_
            .process_sender
            .try_send(thrift_gen::jaeger::Process::new(service_name, tags))
            .ok();

        Ok(())
    }

    /// Queue a span to be reported to local jaeger agent.
    fn report_span(&self, py: Python, py_span: Py<PyAny>) -> PyResult<()> {
        // This may fail if the queue is full. We should probably log something
        // somehow?

        let span: thrift_gen::jaeger::Span = py_span.extract(py)?;

        self.span_sender.try_send(span).ok();

        Ok(())
    }

    fn get_stats(&self) -> Stats {
        let queue_size = self.queue_size.load(Ordering::Relaxed);
        let sent_batches = self.sent_batches.load(Ordering::Relaxed);
        let sent_batches_errors = self.sent_batches_errors.load(Ordering::Relaxed);
        let span_sender_size = self.span_sender.len();
        let process_sender_size = self.process_sender.len();
        let last_error = self.last_error.lock().expect("poisoned").as_ref().cloned();

        Stats {
            queue_size,
            sent_batches,
            sent_batches_errors,
            span_sender_size,
            process_sender_size,
            last_error,
        }
    }
}

/// This is taken from the python jaeger-client class. This is only used by
/// `set_processs`.
fn make_tags(py: Python, dict: &Bound<'_, PyDict>) -> PyResult<Vec<thrift_gen::jaeger::Tag>> {
    let mut tags = Vec::new();

    for (key, value) in dict.iter() {
        let key_str = key.str()?.to_string();
        if let Ok(val) = value.extract::<bool>() {
            tags.push(thrift_gen::jaeger::Tag {
                key: key_str,
                v_type: thrift_gen::jaeger::TagType::Bool,
                v_str: None,
                v_double: None,
                v_bool: Some(val),
                v_long: None,
                v_binary: None,
            });
        } else if let Ok(val) = value.extract::<String>() {
            // The python client truncates strings, presumably so that things
            // fit in a UDP packet.
            let mut string: String = value.str()?.to_string();
            string.truncate(1024);

            tags.push(thrift_gen::jaeger::Tag {
                key: key_str,
                v_type: thrift_gen::jaeger::TagType::String,
                v_str: Some(val),
                v_double: None,
                v_bool: None,
                v_long: None,
                v_binary: None,
            });
        } else if let Ok(val) = value.extract::<f64>() {
            tags.push(thrift_gen::jaeger::Tag {
                key: key_str,
                v_type: thrift_gen::jaeger::TagType::Double,
                v_str: None,
                v_double: Some(OrderedFloat::from(val)),
                v_bool: None,
                v_long: None,
                v_binary: None,
            });
        } else if let Ok(val) = value.extract::<i64>() {
            tags.push(thrift_gen::jaeger::Tag {
                key: key_str,
                v_type: thrift_gen::jaeger::TagType::Long,
                v_str: None,
                v_double: None,
                v_bool: None,
                v_long: Some(val),
                v_binary: None,
            });
        } else if value.get_type().name()? == "traceback" {
            let formatted_traceback = PyString::new_bound(py, "")
                .call_method1("join", (value.call_method0("format")?,))?;

            // The python client truncates strings, presumably so that things
            // fit in a UDP packet.
            let mut string: String = formatted_traceback.extract()?;
            string.truncate(4096);

            tags.push(thrift_gen::jaeger::Tag {
                key: key_str,
                v_type: thrift_gen::jaeger::TagType::String,
                v_str: Some(string),
                v_double: None,
                v_bool: None,
                v_long: None,
                v_binary: None,
            });
        } else {
            // Default to just a stringified version.
            let mut string: String = value.str()?.to_string();
            string.truncate(1024);

            tags.push(thrift_gen::jaeger::Tag {
                key: key_str,
                v_type: thrift_gen::jaeger::TagType::String,
                v_str: Some(string),
                v_double: None,
                v_bool: None,
                v_long: None,
                v_binary: None,
            });
        }
    }

    Ok(tags)
}

/// A wrapper around a UDP socket that implements Write. `UdpSocket::connect`
/// must have been called on the socket.
struct ConnectedUdp {
    socket: UdpSocket,
}

impl Write for ConnectedUdp {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket.send(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Here follows a bunch of implementations to convert the thrift python objects
// to their rust counterparts.

impl FromPyObject<'_> for thrift_gen::jaeger::Process {
    fn extract(ob: &'_ PyAny) -> PyResult<Self> {
        let span = thrift_gen::jaeger::Process {
            service_name: ob.getattr("serviceName")?.extract()?,
            tags: ob.getattr("tags")?.extract()?,
        };

        Ok(span)
    }
}

impl FromPyObject<'_> for thrift_gen::jaeger::Span {
    fn extract_bound(ob: &Bound<'_, PyAny>) -> PyResult<Self> {
        // Annoyingly the jaeger client gives us its own version of Span, rather
        // than the swift version.
        //
        // This is all a bunch of nonesense we've copied from the
        // `jaeger-client` to support large ints.

        let trace_id: u128 = ob.getattr("trace_id")?.extract()?;
        let span_id: u128 = ob.getattr("span_id")?.extract()?;
        let parent_span_id = ob
            .getattr("parent_id")?
            .extract::<Option<u64>>()?
            .unwrap_or_default();
        let flags = ob.getattr("context")?.getattr("flags")?.extract()?;
        let start_time: f64 = ob.getattr("start_time")?.extract()?;
        let end_time: f64 = ob.getattr("end_time")?.extract()?;

        let trace_id_low = (trace_id & ((1 << 64) - 1)) as i64;
        let trace_id_high = ((trace_id >> 64) & ((1 << 64) - 1)) as i64;

        let references = match ob
            .getattr("references")?
            .extract::<Option<Vec<Bound<'_, PyAny>>>>()?
        {
            Some(refs) => {
                let mut encoded_references = Vec::with_capacity(refs.len());

                for reference in refs {
                    let context = reference.getattr("referenced_context")?;
                    let trace_id: u128 = context.getattr("trace_id")?.extract()?;

                    let python_ref_type: PyBackedStr = reference.getattr("type")?.extract()?;
                    let ref_type: thrift_gen::jaeger::SpanRefType = match &*python_ref_type {
                        "follows_from" => thrift_gen::jaeger::SpanRefType::FollowsFrom,
                        "child_of" => thrift_gen::jaeger::SpanRefType::ChildOf,
                        _ => {
                            eprintln!(
                                    "rust-python-jaeger-reporter: unknown reference type {}, defaulting to child_of",
                                    python_ref_type,
                                );
                            thrift_gen::jaeger::SpanRefType::ChildOf
                        }
                    };

                    encoded_references.push(thrift_gen::jaeger::SpanRef {
                        ref_type,
                        trace_id_high: ((trace_id >> 64) & ((1 << 64) - 1)) as i64,
                        trace_id_low: (trace_id & ((1 << 64) - 1)) as i64,
                        span_id: context.getattr("span_id")?.extract::<u64>()? as i64,
                    });
                }

                if !encoded_references.is_empty() {
                    Some(encoded_references)
                } else {
                    None
                }
            }
            None => None,
        };

        let span = thrift_gen::jaeger::Span {
            trace_id_low,
            trace_id_high,
            span_id: span_id as i64, // These converstion from u64 -> i64 do the correct overflow.
            parent_span_id: parent_span_id as i64,
            operation_name: ob.getattr("operation_name")?.extract()?,
            references,
            flags,
            start_time: (start_time * 1000000f64) as i64,
            duration: ((end_time - start_time) * 1000000f64) as i64,
            tags: ob.getattr("tags")?.extract()?,
            logs: ob.getattr("logs")?.extract()?,
        };

        Ok(span)
    }
}

impl FromPyObject<'_> for thrift_gen::jaeger::SpanRef {
    fn extract(ob: &'_ PyAny) -> PyResult<Self> {
        let span = thrift_gen::jaeger::SpanRef {
            ref_type: ob.getattr("refType")?.extract()?,
            trace_id_low: ob.getattr("traceIdLow")?.extract()?,
            trace_id_high: ob.getattr("traceIdHigh")?.extract()?,
            span_id: ob.getattr("spanId")?.extract()?,
        };

        Ok(span)
    }
}

impl FromPyObject<'_> for thrift_gen::jaeger::SpanRefType {
    fn extract(ob: &'_ PyAny) -> PyResult<Self> {
        Ok(
            thrift_gen::jaeger::SpanRefType::try_from(ob.extract::<i32>()?)
                .map_err(|_| PyDowncastError::new(ob, "jaeger::SpanRefType"))?,
        )
    }
}

impl FromPyObject<'_> for thrift_gen::jaeger::Tag {
    fn extract(ob: &'_ PyAny) -> PyResult<Self> {
        let span = thrift_gen::jaeger::Tag {
            key: ob.getattr("key")?.extract()?,
            v_type: ob.getattr("vType")?.extract()?,
            v_str: ob.getattr("vStr")?.extract()?,
            v_double: ob
                .getattr("vDouble")?
                .extract::<Option<f64>>()?
                .map(OrderedFloat),
            v_bool: ob.getattr("vBool")?.extract()?,
            v_long: ob.getattr("vLong")?.extract()?,
            v_binary: ob.getattr("vBinary")?.extract()?,
        };

        Ok(span)
    }
}

impl FromPyObject<'_> for thrift_gen::jaeger::TagType {
    fn extract(ob: &'_ PyAny) -> PyResult<Self> {
        Ok(thrift_gen::jaeger::TagType::try_from(ob.extract::<i32>()?)
            .map_err(|_| PyDowncastError::new(ob, "jaeger::TagType"))?)
    }
}

impl FromPyObject<'_> for thrift_gen::jaeger::Log {
    fn extract(ob: &'_ PyAny) -> PyResult<Self> {
        let span = thrift_gen::jaeger::Log {
            timestamp: ob.getattr("timestamp")?.extract()?,
            fields: ob.getattr("fields")?.extract()?,
        };

        Ok(span)
    }
}

/// A Transport that buffers writes up until `flush()` is called, then will
/// attempt to write the full buffer down the channel at once.
#[derive(Debug)]
pub struct TBufferedTransport<C>
where
    C: Write,
{
    buf: Vec<u8>,
    channel: C,
}

impl<C> TBufferedTransport<C>
where
    C: Write,
{
    pub fn new(channel: C) -> TBufferedTransport<C> {
        TBufferedTransport::with_capacity(4096, channel)
    }

    pub fn with_capacity(write_capacity: usize, channel: C) -> TBufferedTransport<C> {
        TBufferedTransport {
            buf: Vec::with_capacity(write_capacity),
            channel,
        }
    }
}

impl<C> Write for TBufferedTransport<C>
where
    C: Write,
{
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        // We want to make sure the queue doesn't become huge, so we silently
        // drop updates if the buffer is already over
        if self.buf.len() > 100 * 1024 {
            return Ok(b.len());
        }

        self.buf.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Technically a write may not write all the buffer and returns how many
        // bytes were actually written. If that happens here then it means we've
        // (probably) sent out a truncated UDP packet, and we don't want to send
        // out the other half of the buffer as a separate packet.
        //
        // Similarly, we still want to drop the buffer and flush even if we get
        // an error.
        let write_result = self.channel.write(&self.buf);

        self.buf.clear();

        // We shrink the capacity of the vector if it gets "big"
        if self.buf.capacity() > 4096 {
            self.buf.shrink_to_fit();
        }

        let flush_result = self.channel.flush();

        // If `write` or `flush` failed we return the error now.
        write_result?;
        flush_result?;

        Ok(())
    }
}

#[cfg(tests)]
mod tests {
    use super::*;

    /// Test that TBufferedTransport functions when calling write/flush correctly.
    #[test]
    fn test_buffered_transport_simple() {
        let mut buf = Vec::new();

        {
            let mut buffered_transport = TBufferedTransport::new(Cursor::new(&mut buf));

            buffered_transport.write_all(b"He").unwrap();
            buffered_transport.write_all(b"llo").unwrap();
            buffered_transport.flush().unwrap();
        }

        assert_eq!(b"Hello", &*buf);
    }

    /// Test that TBufferedTransport doesn't duplicate data on repeated calls.
    #[test]
    fn test_buffered_transport_no_duplicate() {
        let mut buf = Vec::new();

        {
            let mut buffered_transport = TBufferedTransport::new(Cursor::new(&mut buf));

            buffered_transport.write_all(b"He").unwrap();
            buffered_transport.write_all(b"llo").unwrap();
            buffered_transport.flush().unwrap();

            buffered_transport.write_all(b" World").unwrap();
            buffered_transport.flush().unwrap();
        }

        assert_eq!(b"Hello World", &*buf);
    }

    /// Test that TBufferedTransport doesn't write data until it flushes.
    #[test]
    fn test_buffered_transport_buffers() {
        let mut buf = Vec::new();

        {
            let mut buffered_transport = TBufferedTransport::new(Cursor::new(&mut buf));

            buffered_transport.write_all(b"He").unwrap();
            buffered_transport.write_all(b"llo").unwrap();
            buffered_transport.flush().unwrap();

            // We write but don't flush.
            buffered_transport.write_all(b" World").unwrap();
        }

        assert_eq!(b"Hello", &*buf);
    }
}
