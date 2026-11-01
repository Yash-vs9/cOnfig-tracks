FROM halloumee/speed-demon-rust:1.99.1-trixie-llvm-tools@sha256:13e4ee7135d8eac6c56476c7d1aee201f36479f4745fd5d0e6b4529f4e13251a AS build
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
RUN RUSTFLAGS="-C target-cpu=native -C profile-generate=/tmp/pgo" cargo build --release --offline && \
    python3 -c 'f=open("/tmp/train.log","w"); [f.write(f"2026-10-30T{10+i%12:02d}:12:34.567890Z GET /api/v1/item_{i%200} {503 if i%7==0 else 200} {1200000 if i%300==0 else 150+i%9000}\n") for i in range(200000)]; f.close()' && \
    ./target/release/speed_demon_a /tmp/train.log /tmp/out.txt && \
    llvm-profdata merge -o /tmp/merged.profdata /tmp/pgo && \
    RUSTFLAGS="-C target-cpu=native -C profile-use=/tmp/merged.profdata -C llvm-args=-pgo-warn-missing-function" cargo build --release --offline && \
    mkdir -p /app && cp target/release/speed_demon_a /app/solution

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
COPY --from=build /app/solution /app/solution
ENTRYPOINT ["/app/solution"]
