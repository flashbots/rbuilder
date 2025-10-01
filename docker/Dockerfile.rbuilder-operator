FROM rust:1.88.0-bullseye@sha256:b315f988b86912bafa7afd39a6ded0a497bf850ec36578ca9a3bdd6a14d5db4e AS builder

ARG BUILD_PROFILE=release
ENV BUILD_PROFILE=$BUILD_PROFILE

RUN apt-get update && apt-get install -y \
    libclang-dev=1:11.0-51+nmu5 \
    protobuf-compiler=3.12.4-1+deb11u1 \
    cmake

# Clone the repository at the specific branch
WORKDIR /app
COPY ./ /app

# Build the project with the reproducible settings
RUN make build-reproducible

RUN mv /app/target/x86_64-unknown-linux-gnu/"${BUILD_PROFILE}"/rbuilder /rbuilder

FROM gcr.io/distroless/cc-debian12:nonroot-6755e21ccd99ddead6edc8106ba03888cbeed41a
COPY --from=builder /rbuilder /rbuilder
ENTRYPOINT [ "/rbuilder" ]
