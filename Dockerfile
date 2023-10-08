FROM ubuntu
COPY ./target/x86_64-unknown-linux-gnu/release/xiu /usr/bin/xiu
ENTRYPOINT ["xiu"]