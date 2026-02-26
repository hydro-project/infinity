# Build stage: compile the Rust binary with cargo-lambda
FROM ghcr.io/cargo-lambda/cargo-lambda AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN cargo lambda build --release --arm64 \
    --manifest-path crates/sandbox-remote/Cargo.toml \
    --bin sandbox-remote

# Runtime stage: Lambda base image with git + jj + our binary
FROM public.ecr.aws/lambda/provided:al2023-arm64

# Install git and tar (tar needed to extract jj release archive)
RUN dnf install -y git tar && dnf clean all

# Trust all repos under the EFS mount so git doesn't complain about ownership
RUN git config --system --add safe.directory '/mnt/efs/sandbox-repos/*'

# Install jj
ARG JJ_VERSION=0.38.0
RUN curl -fsSL "https://github.com/jj-vcs/jj/releases/download/v${JJ_VERSION}/jj-v${JJ_VERSION}-aarch64-unknown-linux-musl.tar.gz" \
    | tar xz -C /usr/local/bin --strip-components=0 ./jj && chmod +x /usr/local/bin/jj

# Copy the compiled binary
COPY --from=builder /build/target/lambda/sandbox-remote/bootstrap ${LAMBDA_RUNTIME_DIR}/bootstrap

ENV XDG_CONFIG_HOME /tmp

CMD ["bootstrap"]
