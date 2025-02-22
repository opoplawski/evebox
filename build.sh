#! /bin/bash

set -e

RELEASE="no"
LATEST="no"

if [ "${REGISTRY}" = "" ]; then
    REGISTRY="docker.io"
fi

DOCKER_NAME="${REGISTRY}/jasonish/evebox"

BUILD_REV=$(git rev-parse --short HEAD)
export BUILD_REV

VERSION=$(cat Cargo.toml | awk '/^version/ { gsub(/"/, "", $3); print $3 }')
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD)

# Set the container tag prefix to "dev" if not on the master branch.
if [ "${DOCKER_TAG_PREFIX}" = "" ]; then
    if [ "${GIT_BRANCH}" = "master" ]; then
        DOCKER_TAG_PREFIX="master"
    else
        DOCKER_TAG_PREFIX="dev"
    fi
fi

echo "BUILD_REV=${BUILD_REV}"

build_webapp() {
    DOCKERFILE="./docker/builder/Dockerfile.cross"
    TAG=${BUILDER_TAG:-"evebox/builder:webapp"}
    docker build --rm \
           --build-arg REAL_UID="$(id -u)" \
           --build-arg REAL_GID="$(id -g)" \
           --cache-from ${TAG} \
	   -t ${TAG} \
	   -f ${DOCKERFILE} .
    docker run ${IT} --rm \
           -v "$(pwd):/src:z" \
           -w /src/webapp \
           -e REAL_UID="$(id -u)" \
           -e REAL_GID="$(id -g)" \
           -e BUILD_REV="${BUILD_REV}" \
           ${TAG} make
}

build_cross() {
    target="$1"
    if [ "${target}" = "" ]; then
        echo "error: target must be set for build_cross"
        exit 1
    fi
    what="$2"
    DOCKERFILE="./docker/builder/Dockerfile.cross"
    TAG=${BUILDER_TAG:-"evebox/builder:cross"}
    sudo rm -rf target
    docker build --rm \
           --cache-from ${TAG} \
	   -t ${TAG} \
	   -f ${DOCKERFILE} .
    docker run ${IT} --rm \
         -v "$(pwd)/target/docker/${TARGET}:/src/target:z" \
         -v "$(pwd)/dist:/src/dist:z" \
         -v /var/run/docker.sock:/var/run/docker.sock \
         -w /src \
         -e REAL_UID="$(id -u)" \
         -e REAL_GID="$(id -g)" \
         -e BUILD_REV="${BUILD_REV}" \
         -e TARGET="${target}" \
         ${TAG} make $what
}

build_linux() {
    build_cross x86_64-unknown-linux-musl "dist rpm deb"
}

build_linux_armv8() {
    build_cross aarch64-unknown-linux-musl dist
}

build_linux_armv7() {
    build_cross armv7-unknown-linux-musleabihf dist
}

build_windows() {
    build_cross x86_64-pc-windows-gnu dist
}

build_macos() {
    TAG=${BUILDER_TAG:-"evebox/builder:macos"}
    DOCKERFILE="./docker/builder/Dockerfile.macos"
    TARGET="x86_64-apple-darwin"
    docker build --rm \
           --build-arg REAL_UID="$(id -u)" \
           --build-arg REAL_GID="$(id -g)" \
           --cache-from ${TAG} \
	   -t ${TAG} \
	   -f ${DOCKERFILE} .
    docker run ${IT} --rm \
           -v "$(pwd)/target/docker/${TARGET}:/src/target:z" \
           -v "$(pwd)/dist:/src/dist:z" \
           -w /src \
           -e REAL_UID="$(id -u)" \
           -e REAL_GID="$(id -g)" \
           -e CC=o64-clang \
           -e TARGET=${TARGET} \
           -e BUILD_REV="${BUILD_REV}" \
           ${TAG} make dist
}

build_docker() {
    if test -e ./dist/evebox-${VERSION}-linux-x64/evebox; then
        version=${VERSION}
    else
        version="latest"
    fi

    docker build \
	   --build-arg "BASE=amd64/alpine" \
           --build-arg "SRC=./dist/evebox-${version}-linux-x64/evebox" \
           -t ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-amd64 \
           -f docker/Dockerfile .

    docker build \
	   --build-arg "BASE=arm32v7/alpine" \
           --build-arg "SRC=./dist/evebox-${version}-linux-arm/evebox" \
           -t ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm32v7 \
           -f docker/Dockerfile .

    docker build \
	   --build-arg "BASE=arm64v8/alpine" \
           --build-arg "SRC=./dist/evebox-${version}-linux-arm64/evebox" \
           -t ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm64v8 \
           -f docker/Dockerfile .
}

docker_push() {
    docker push ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-amd64
    docker push ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm32v7
    docker push ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm64v8

    docker manifest create -a ${DOCKER_NAME}:${DOCKER_TAG_PREFIX} \
           ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-amd64 \
           ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm32v7 \
           ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm64v8
    docker manifest annotate --arch arm --variant v7 \
           ${DOCKER_NAME}:${DOCKER_TAG_PREFIX} \
           ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm32v7
    docker manifest push --purge ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}

    if [ "${LATEST}" = "yes" ]; then
        docker manifest create -a ${DOCKER_NAME}:latest \
               ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-amd64 \
               ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm32v7 \
               ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm64v8
        docker manifest annotate --arch arm --variant v7 \
               ${DOCKER_NAME}:latest \
               ${DOCKER_NAME}:${DOCKER_TAG_PREFIX}-arm32v7
        docker manifest push --purge ${DOCKER_NAME}:latest
    fi
}

for arg in $@; do
    case "${arg}" in
        --release)
            RELEASE="yes"
            shift
            ;;
        --latest)
            LATEST="yes"
            shift
            ;;
    esac
done

if [ "${RELEASE}" = "yes" ]; then
    DOCKER_TAG_PREFIX="${VERSION}"
fi

case "$1" in
    webapp)
        build_webapp
        ;;

    linux)
        build_linux
        ;;

    linux-arm32)
        build_linux_armv7
        ;;

    linux-arm64)
        build_linux_armv8
        ;;

    windows)
        build_windows
        ;;

    macos)
        build_macos
        ;;

    docker)
        build_docker
        ;;

    docker-push)
        build_docker
        docker_push
        ;;

    all)
        build_webapp
        build_linux
        build_linux_armv7
        build_linux_armv8
        build_windows
        build_macos
        build_docker
        ;;

    *)
        cat <<EOF
usage: $0 <command>

Commands:
    webapp
    linux
    linux-arm
    windows
    macos
    docker
    docker-push
    all
EOF
        exit 1
        ;;
esac
