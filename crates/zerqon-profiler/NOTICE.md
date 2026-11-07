# Native profiler and spark format

The native sampler is Zerqon Rust code using `perf-event2` (MIT OR Apache-2.0) and `backtrace` (MIT OR Apache-2.0). It measures Linux user-space CPU stacks using kernel `perf_event_open`; it is not the Java spark sampler or async-profiler.

The binary exporter implements the public spark SamplerData wire format from [lucko/spark, revision 7ffae3d7ceb69df95480fd03e3f89d979c780fd8](https://github.com/lucko/spark/tree/7ffae3d7ceb69df95480fd03e3f89d979c780fd8/spark-common/src/main/proto/spark). Spark is copyright its upstream authors and distributed under GPL-3.0. Zerqon preserves this attribution and uses its own GPL-3.0-only project license. The official protocol schema and test compiler are fetched into `.cache/` for independent decoding checks; no Java spark implementation is bundled or renamed.

Metadata uses unknown sampler-engine enum value 2 and identifies `Linux perf_event_open CPU-clock` explicitly. It does not claim the official JAVA or ASYNC engines. The official protobuf decoder preserves this value; native-engine labeling is not an upstream spark extension guarantee.
