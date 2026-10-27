FROM golang:1.26-bookworm AS build
WORKDIR /src
COPY go.mod go.sum ./
ARG GOPROXY=https://proxy.golang.org,direct
RUN go mod download
COPY *.go ./
COPY web ./web
ARG VERSION=dev
RUN go test -race ./... && CGO_ENABLED=1 go build -trimpath -buildmode=c-shared \
    -ldflags "-X main.pluginVersion=${VERSION}" -o /out/omp-gateway-usage.so .

FROM scratch
COPY --from=build /out/omp-gateway-usage.so /omp-gateway-usage.so
