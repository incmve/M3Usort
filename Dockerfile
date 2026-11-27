# ---- build ----
FROM rust:1-slim-trixie AS builder
WORKDIR /src

# cargo-auditable embeds the dependency list in the binary, so Trivy (Arcane's
# scanner) can see which crates are inside and report CVEs in them.
RUN cargo install cargo-auditable --locked

# Cache dependencies separately from the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo auditable build --release --locked \
    && rm -rf src

COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
# Commit shown in the footer and `m3usort version`; set by CI.
ARG M3USORT_GIT_SHA=dev
RUN touch src/main.rs && cargo auditable build --release --locked

# ---- runtime ----
# distroless/cc: glibc + libgcc, no shell, no package manager.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /src/target/release/m3usort /usr/local/bin/m3usort

ENV M3USORT_DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 5050

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/m3usort", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/m3usort"]
CMD ["serve"]
