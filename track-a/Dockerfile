FROM halloumee/speed-demon-rust:1.99.1-trixie-llvm-tools@sha256:13e4ee7135d8eac6c56476c7d1aee201f36479f4745fd5d0e6b4529f4e13251a AS build
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src

# Step 1: Instrument with PGO
RUN RUSTFLAGS="-C target-cpu=native -C profile-generate=/tmp/pgo" cargo build --release --offline

# Step 2: Generate training workload and profile hot loops
RUN printf '%s\n' \
'methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"]' \
'endpoints = [f"/api/v1/resource/{i}/sub/{i*2}" for i in range(250)] + ["/health", "/status", "/"]' \
'with open("/tmp/train.log", "w") as f:' \
'    for i in range(50000):' \
'        h = 18 + (i % 6)' \
'        st = "500" if i % 20 == 0 else "200"' \
'        lat = 500 if i % 10 == 0 else (1200000 if i % 50 == 0 else 15000 + (i % 40000))' \
'        f.write(f"2026-10-30T{h:02d}:00:00.000000Z {methods[i%7]} {endpoints[i%len(endpoints)]} {st} {lat}\n")' \
> /tmp/gen.py && \
python3 /tmp/gen.py && \
mkdir -p /tmp/out && \
./target/release/speed_demon_a /tmp/train.log /tmp/out/summary.txt && \
llvm-profdata merge -o /tmp/pgo/merged.profdata /tmp/pgo

# Step 3: Compile with profile feedback
RUN RUSTFLAGS="-C target-cpu=native -C profile-use=/tmp/pgo/merged.profdata" cargo build --release --offline && \
    mkdir -p /app && cp target/release/speed_demon_a /app/solution

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
COPY --from=build /app/solution /app/solution
ENTRYPOINT ["/app/solution"]
