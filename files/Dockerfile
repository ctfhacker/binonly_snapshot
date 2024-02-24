###################################################
#### Ubuntu root FS
FROM ubuntu:jammy as base
RUN apt-get update -q \
  && apt-get install -q -y build-essential clang gdb python3 \
  && apt-get clean -y \
  && rm -rf /var/lib/apt/lists/*

#### build harness
COPY $BINARY$ /root/

###################################################
FROM snapchange_snapshot

COPY --from=base / "$SNAPSHOT_INPUT"

ENV SNAPSHOT_ENTRYPOINT=/root/$BINARYNAME$
