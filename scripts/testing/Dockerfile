# Public Linux test environment: no Android SDK, credentials or host services.
FROM rust:1.95-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang cmake curl git libgl1 libglu1-mesa libgtk-3-dev liblzma-dev \
    ninja-build openssh-client openssl pkg-config python3 python3-venv \
    unzip xz-utils ca-certificates && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt clippy \
    && curl -LsSf https://astral.sh/uv/0.10.2/install.sh | env UV_INSTALL_DIR=/usr/local/bin sh
RUN git clone --depth 1 --branch 3.47.9 https://github.com/flutter/flutter.git /opt/flutter \
    && git config --system --add safe.directory /opt/flutter
ENV PATH="/opt/flutter/bin:${PATH}" \
    FLUTTER_ROOT=/opt/flutter \
    CARGO_PROFILE_DEV_DEBUG=0 \
    CARGO_PROFILE_TEST_DEBUG=0 \
    CARGO_INCREMENTAL=0
RUN flutter config --no-analytics && flutter precache --linux
ENV UV_PYTHON_INSTALL_DIR=/opt/python UV_PYTHON_BIN_DIR=/usr/local/bin
RUN uv python install 3.13 --default
WORKDIR /workspace
CMD ["./scripts/test.sh"]
