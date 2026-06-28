# Build the operator from the workspace root:
#   docker build -f crates/catalerum-operator/Dockerfile -t catalerum-operator:dev .
FROM rust:1.85-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p catalerum-operator

# Distroless runtime; the operator needs no shell and runs non-root + RO rootfs.
FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/catalerum-operator /usr/local/bin/catalerum-operator
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/catalerum-operator", "run"]
