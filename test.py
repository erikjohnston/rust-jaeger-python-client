from jaeger_client import Config
import opentracing
from opentracing import tags

from pympler.asizeof import asizeof
from pympler import summary
from pympler import muppy
from pympler import tracker

import gc
import random
import time
import objgraph

from rust_python_jaeger_reporter import Reporter

if __name__ == "__main__":
    import logging

    log_level = logging.DEBUG
    logging.getLogger("").handlers = []
    logging.basicConfig(format="%(asctime)s %(message)s", level=log_level)

    reporter = Reporter()

    config = Config(
        config={  # usually read from some yaml config
            "sampler": {
                "type": "const",
                "param": 1,
            },
            # 'logging': True,
        },
        service_name="rust-jaeger-python-client-test",
    )

    tracer = config.create_tracer(reporter, config.sampler)
    # tracer = config.initialize_tracer()
    opentracing.set_global_tracer(tracer)

    i = 0
    # for _ in range(100000):
    # tr = tracker.SummaryTracker()

    # tr = tracker.SummaryTracker()

    while True:
        with tracer.start_span("TestSpan") as span:

            span.set_tag("test_int", 25)
            span.set_tag("test_str", "foobar")
            span.set_tag("test_double", 1.25)
            span.set_tag("test_bool", True)

            with tracer.start_span(
                "ChildSpan",
                references=opentracing.follows_from(span.context),
            ) as span2:
                pass

        i += 1
        if i % 10000 == 0:
            # print(reporter.get_stats())
            gc.collect()
            # print(len(objgraph.get_leaking_objects()))

            # all_objects = muppy.get_objects()
            # print("objects", len(all_objects))
            # tr.print_diff()

            # roots = objgraph.get_leaking_objects()
            # print("Roots", len(roots))

            # print(tr.diff())
            # tr.print_diff()

            # objgraph.show_backrefs(
            #     random.sample(objgraph.by_type('list'), 20),
            #     max_depth=5,
            #     filename='chain.dot')

            # print(gc.garbage)
            # print(asizeof(reporter))
            # print(asizeof(tracer))
            # print(asizeof(opentracing))
