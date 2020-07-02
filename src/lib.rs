use crossbeam_channel::{bounded, Sender};
use ordered_float::OrderedFloat;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};
use pyo3::PyDowncastError;
use thrift::protocol::{TCompactInputProtocol, TCompactOutputProtocol};
use try_from::TryFrom;

use std::io::empty;
use std::io::{self, Write};
use std::mem;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

mod thrift_gen;

use crate::thrift_gen::agent::TAgentSyncClient;

#[pymodule]
/// The root Python module.
fn rust_python_jaeger_reporter(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<Reporter>()?;

    Ok(())
}

/// The main reporter class.
#[pyclass]
#[text_signature = "()"]
struct Reporter {
    span_sender: Sender<thrift_gen::jaeger::Span>,
    process_sender: Sender<thrift_gen::jaeger::Process>,
}

#[pymethods]
impl Reporter {
    #[new]
    fn new() -> PyResult<Reporter> {
        // Set up the UDP transport
        let socket = UdpSocket::bind(
            &(49152..65535)
                .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
                .collect::<Vec<_>>()[..],
        )?;
        socket.connect("127.0.0.1:6831")?;

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
        let (span_sender, span_receiver) = bounded(1000);
        let (process_sender, process_receiver) = bounded(1000);

        std::thread::Builder::new()
            .name("jaeger_sender".to_string())
            .spawn(move || {
                let mut queue = Vec::with_capacity(100);
                let mut process = None;

                let mut last_push = Instant::now();

                loop {
                    // Wait for new span to be queud.
                    if let Ok(span) = span_receiver.recv_timeout(Duration::from_secs(5)) {
                        queue.push(span);
                    }

                    // Check if we have been given any new process information
                    // since the last loop.
                    while let Ok(new_process) = process_receiver.try_recv() {
                        process = Some(new_process);
                    }

                    // We batch up the spans before sending them, waiting at
                    // most N seconds between sends
                    if queue.len() > 50 || (!queue.is_empty() && last_push.elapsed().as_secs() > 20)
                    {
                        last_push = Instant::now();

                        if let Some(process) = process.clone() {
                            let to_send = mem::replace(&mut queue, Vec::with_capacity(100));
                            agent
                                .emit_batch(thrift_gen::jaeger::Batch::new(process, to_send))
                                .ok();
                        }
                    }
                }
            })
            .unwrap();

        Ok(Reporter {
            process_sender,
            span_sender,
        })
    }

    /// Sets the process information needed to report spans.
    #[text_signature = "($self, service_name, tags, max_length, /)"]
    fn set_process(
        self_: PyRef<Self>,
        service_name: String,
        tags: &PyDict,
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
    #[text_signature = "($self, span, /)"]
    fn report_span(self_: PyRef<Self>, span: thrift_gen::jaeger::Span) {
        // This may fail if the queue is full. We should probably log something
        // somehow?
        self_.span_sender.try_send(span).ok();
    }
}

/// This is taken from the python jaeger-client class. This is only used by
/// `set_processs`.
fn make_tags(py: Python, dict: &PyDict) -> PyResult<Vec<thrift_gen::jaeger::Tag>> {
    let mut tags = Vec::new();

    for (key, value) in dict.iter() {
        let key_str = key.str()?.to_string()?.to_string();
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
            let mut string: String = value.str()?.to_string()?.to_string();
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
        } else if value.get_type().name() == "traceback" {
            let formatted_traceback =
                PyString::new(py, "").call_method1("join", (value.call_method0("format")?,))?;

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
            let mut string: String = value.str()?.to_string()?.to_string();
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

/// An extension trait that extracts attributes from a python object. This gives
/// better error messages than doing `.getattr(..).extract()` as it reports
/// which attribute we failed to parse.
trait ExtractAttribute {
    fn extract_attribute<'a, D>(&'a self, attriubte: &str) -> PyResult<D>
    where
        D: FromPyObject<'a>;
}

impl ExtractAttribute for &PyAny {
    fn extract_attribute<'a, D>(&'a self, attriubte: &str) -> PyResult<D>
    where
        D: FromPyObject<'a>,
    {
        FromPyObject::extract(self.getattr(attriubte)?).map_err(|err| {
            PyErr::new::<pyo3::exceptions::TypeError, _>((
                format!("Failed to extract attribute '{}'", attriubte),
                err,
            ))
        })
    }
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

impl<'a> FromPyObject<'a> for thrift_gen::jaeger::Process {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        let span = thrift_gen::jaeger::Process {
            service_name: ob.getattr("serviceName")?.extract()?,
            tags: ob.getattr("tags")?.extract()?,
        };

        Ok(span)
    }
}

impl<'a> FromPyObject<'a> for thrift_gen::jaeger::Span {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        // Annoyingly the jaeger client gives us its own version of Span, rather
        // than the swift version.
        //
        // This is all a bunch of nonesense we've copied from the
        // `jaeger-client` to support large ints.

        let trace_id: u128 = ob.extract_attribute("trace_id")?;
        let span_id: u128 = ob.extract_attribute("span_id")?;
        let parent_span_id = ob
            .extract_attribute::<Option<u64>>("parent_id")?
            .unwrap_or_default();
        let flags = ob.getattr("context")?.extract_attribute("flags")?;
        let start_time: f64 = ob.extract_attribute("start_time")?;
        let end_time: f64 = ob.extract_attribute("end_time")?;

        let trace_id_high = (trace_id & ((1 << 64) - 1)) as i64;
        let trace_id_low = ((trace_id >> 64) & ((1 << 64) - 1)) as i64;

        let references = match ob.extract_attribute::<Option<Vec<&PyAny>>>("references")? {
            Some(refs) => {
                let mut encoded_references = Vec::with_capacity(refs.len());

                for reference in refs {
                    let context = reference.getattr("referenced_context")?;
                    let trace_id: u128 = context.extract_attribute("trace_id")?;
                    encoded_references.push(thrift_gen::jaeger::SpanRef {
                        ref_type: match reference.extract_attribute("type")? {
                            "FOLLOWS_FROM" => thrift_gen::jaeger::SpanRefType::FollowsFrom,
                            _ => thrift_gen::jaeger::SpanRefType::ChildOf,
                        },
                        trace_id_low: ((trace_id >> 64) & ((1 << 64) - 1)) as i64,
                        trace_id_high: (trace_id & ((1 << 64) - 1)) as i64,
                        span_id: context.extract_attribute::<u64>("span_id")? as i64,
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
            operation_name: ob.extract_attribute("operation_name")?,
            references,
            flags,
            start_time: (start_time * 1000000f64) as i64,
            duration: ((end_time - start_time) * 1000000f64) as i64,
            tags: ob.extract_attribute("tags")?,
            logs: ob.extract_attribute("logs")?,
        };

        Ok(span)
    }
}

impl<'a> FromPyObject<'a> for thrift_gen::jaeger::SpanRef {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        let span = thrift_gen::jaeger::SpanRef {
            ref_type: ob.getattr("refType")?.extract()?,
            trace_id_low: ob.getattr("traceIdLow")?.extract()?,
            trace_id_high: ob.getattr("traceIdHigh")?.extract()?,
            span_id: ob.getattr("spanId")?.extract()?,
        };

        Ok(span)
    }
}

impl<'a> FromPyObject<'a> for thrift_gen::jaeger::SpanRefType {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        Ok(
            thrift_gen::jaeger::SpanRefType::try_from(ob.extract::<i32>()?)
                .map_err(|_| PyDowncastError)?,
        )
    }
}

impl<'a> FromPyObject<'a> for thrift_gen::jaeger::Tag {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
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

impl<'a> FromPyObject<'a> for thrift_gen::jaeger::TagType {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        Ok(thrift_gen::jaeger::TagType::try_from(ob.extract::<i32>()?)
            .map_err(|_| PyDowncastError)?)
    }
}

impl<'a> FromPyObject<'a> for thrift_gen::jaeger::Log {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
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
        let _ = self.channel.write(&self.buf)?;

        // We shrink the capacity of the vector if it gets "big"
        if self.buf.capacity() > 4096 {
            self.buf.clear();
            self.buf.shrink_to_fit();
        }

        self.channel.flush()
    }
}
