FROM rust:1.97-slim AS builder
# rusqlite bundled 需要用 C 编译器构建内置 SQLite
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/search-x /usr/local/bin/search-x
# 挂载目录而非单文件：单文件 bind mount 在宿主机文件被 rename 替换（sed -i / vim）后
# 容器内仍指向旧 inode，热重载会读到旧配置。
ENV SEARCH_X_CONFIG=/config/config.toml
EXPOSE 3088
CMD ["search-x"]
