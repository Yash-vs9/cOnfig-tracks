FROM halloumee/speed-demon-rust:1.99.1-trixie-llvm-tools@sha256:13e4ee7135d8eac6c56476c7d1aee201f36479f4745fd5d0e6b4529f4e13251a AS build
WORKDIR /src
COPY main.rs .
RUN rustc -C opt-level=3 -C target-cpu=native -C target-feature=+avx2,+sse4.2,+bmi1,+bmi2 -C codegen-units=1 -C lto=fat -C panic=abort -C strip=symbols --edition=2024 main.rs -o /usr/local/bin/solution

FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
COPY --from=build /usr/local/bin/solution /usr/local/bin/solution
ENTRYPOINT ["/usr/local/bin/solution"]
