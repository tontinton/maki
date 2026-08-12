FROM docker.io/library/rust:1.97.1-slim-bookworm AS build

COPY . /maki/
WORKDIR /maki/

RUN apt-get update \
  && apt-get install --yes perl make g++

RUN cargo build --release

RUN apt-get autoremove --yes \
  && apt-get clean --yes \
  && rm --recursive --force /tmp/* /var/tmp/* \
  && find /var/cache/apt/archives /var/lib/apt/lists -not -name lock -type f -delete \
  && find /var/cache -type f -delete

FROM docker.io/library/ubuntu:26.04 AS runtime

RUN apt-get update \
  && apt-get install --yes git vim

COPY --from=build /maki/target/release/maki /usr/local/bin/

RUN apt-get autoremove --yes \
  && apt-get clean --yes \
  && rm --recursive --force /tmp/* /var/tmp/* \
  && find /var/cache/apt/archives /var/lib/apt/lists -not -name lock -type f -delete \ 
  && find /var/cache -type f -delete

ENTRYPOINT ["/usr/local/bin/maki"]
