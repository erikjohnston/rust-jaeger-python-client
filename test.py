from jaeger_client import Config
import opentracing


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

    for _ in range(100000):
        with tracer.start_span('TestSpan') as span:
            pass
