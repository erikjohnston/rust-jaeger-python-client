Rust Jaeger Python Client
=========================

[![PyPI](https://img.shields.io/pypi/v/maturin.svg)](https://pypi.org/project/rust-python-jaeger-reporter/)
[![PyPI - Format](https://img.shields.io/pypi/format/rust-python-jaeger-reporter)](https://pypi.org/project/rust-python-jaeger-reporter/)

A faster reporter for the python [`jaeger-client`](https://pypi.org/project/jaeger-client/) that reports spans in a native background thread.

This is relatively untested, so use at your own risk! (You may want to manually wrap this class in python so that calls to `report_span` cannot fail).

Usage:

```python
from jaeger_client import Config
import opentracing

from rust_python_jaeger_reporter import Reporter

# The standard config for jaeger. No need to change anything here.
config = Config(
    config={
        'sampler': {
            'type': 'const',
            'param': 1,
        },
    },
    service_name='your-app-name',
)

# Create the rust reporter.
reporter = Reporter()

# Create the tracer and install it as the global tracer.
#
# *Note*: This invocation doesn't support throttling or the remote sampler.
tracer = config.create_tracer(reporter, config.sampler)
opentracing.set_global_tracer(tracer)

```

Limitations
-----------

The reporter is not configurable and is hardcoded to report to the local agent
on localhost and the default port.


Building
--------

Requires a nightly rust compiler, due to using the PyO3 library.
[Maturin](https://github.com/PyO3/maturin) can be used to develop, test and
publish the library.


Publishing to PyPI
------------------

As per the [maturin docs](https://github.com/PyO3/maturin#manylinux-and-auditwheel)
we use a docker image to build the binary wheels for the various python versions:

```
docker run -it --rm -v $(pwd):/io konstin2/maturin publish
```
