from jaeger_client import Config
import opentracing
from opentracing import tags


from rust_python_jaeger_reporter import Reporter

if __name__ == "__main__":
    import logging
    log_level = logging.DEBUG
    logging.getLogger('').handlers = []
    logging.basicConfig(format='%(asctime)s %(message)s', level=log_level)

    reporter = Reporter()

    config = Config(
        config={  # usually read from some yaml config
            'sampler': {
                'type': 'const',
                'param': 1,
            },
            # 'logging': True,
        },
        service_name='your-app-name',
    )

    tracer = config.create_tracer(reporter, config.sampler)
    opentracing.set_global_tracer(tracer)

    for _ in range(10000):
        with tracer.start_span('TestSpan') as span:

            span.set_tag("test_int", 25)
            span.set_tag("test_str", "foobar")
            span.set_tag("test_double", 1.25)
            span.set_tag("test_bool", True)

            with tracer.start_span('TestSpan2', child_of=span) as span2:
                pass
