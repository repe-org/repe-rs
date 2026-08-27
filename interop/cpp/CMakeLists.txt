# Builds the REPE interop fixture generator against the canonical C++
# implementation (Glaze). By default Glaze is fetched at a pinned tag so the
# committed fixtures and the CI drift check build against the exact same
# source. Point -DGLAZE_SOURCE_DIR=/path/to/glaze at a local checkout to
# iterate without a network fetch.
#
#   cmake -S interop/cpp -B interop/cpp/build [-DGLAZE_SOURCE_DIR=/path/to/glaze]
#   cmake --build interop/cpp/build
#   interop/cpp/build/generate_fixtures interop/fixtures v8.0.0

cmake_minimum_required(VERSION 3.21)
project(repe_interop_fixtures CXX)

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

set(GLAZE_SOURCE_DIR "" CACHE PATH "Path to a local Glaze checkout (overrides the FetchContent fetch)")
set(GLAZE_GIT_TAG "v8.0.0" CACHE STRING "Glaze git tag to fetch when GLAZE_SOURCE_DIR is unset")

if(GLAZE_SOURCE_DIR)
    message(STATUS "Using local Glaze checkout: ${GLAZE_SOURCE_DIR}")
    add_subdirectory(${GLAZE_SOURCE_DIR} ${CMAKE_BINARY_DIR}/glaze-local)
else()
    message(STATUS "Fetching Glaze ${GLAZE_GIT_TAG} via FetchContent")
    include(FetchContent)
    FetchContent_Declare(
        glaze
        GIT_REPOSITORY https://github.com/stephenberry/glaze.git
        GIT_TAG ${GLAZE_GIT_TAG}
        GIT_SHALLOW TRUE
    )
    FetchContent_MakeAvailable(glaze)
endif()

add_executable(generate_fixtures generate_fixtures.cpp)
target_link_libraries(generate_fixtures PRIVATE glaze::glaze)

# Plugin-ABI host: dlopens a Rust plugin and drives it through
# glaze/rpc/repe/plugin.h. Shares this project's Glaze so both interop halves —
# wire format and plugin ABI — are pinned to one tag.
#
#   cargo build --release --features plugin --example repe_plugin
#   interop/cpp/build/plugin_host target/release/examples/librepe_plugin.{so,dylib}
add_executable(plugin_host plugin_host.cpp)
target_link_libraries(plugin_host PRIVATE glaze::glaze ${CMAKE_DL_LIBS})

# The mirror: a C++ Glaze plugin for the *Rust* host to dlopen. MODULE rather
# than SHARED because nothing links against it — it is only ever loaded at
# runtime, which is also what gives it the platform's plugin naming.
#
#   cargo build --release --features plugin-host --example plugin_host
#   target/release/examples/plugin_host interop/cpp/build/libexample_plugin.{so,dylib}
add_library(example_plugin MODULE example_plugin.cpp)
target_link_libraries(example_plugin PRIVATE glaze::glaze)
set_target_properties(example_plugin PROPERTIES PREFIX "lib")
