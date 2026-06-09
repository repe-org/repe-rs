# Builds the REPE interop fixture generator against the canonical C++
# implementation (Glaze). By default Glaze is fetched at a pinned tag so the
# committed fixtures and the CI drift check build against the exact same
# source. Point -DGLAZE_SOURCE_DIR=/path/to/glaze at a local checkout to
# iterate without a network fetch.
#
#   cmake -S interop/cpp -B interop/cpp/build [-DGLAZE_SOURCE_DIR=/path/to/glaze]
#   cmake --build interop/cpp/build
#   interop/cpp/build/generate_fixtures interop/fixtures v7.7.1

cmake_minimum_required(VERSION 3.21)
project(repe_interop_fixtures CXX)

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

set(GLAZE_SOURCE_DIR "" CACHE PATH "Path to a local Glaze checkout (overrides the FetchContent fetch)")
set(GLAZE_GIT_TAG "v7.7.1" CACHE STRING "Glaze git tag to fetch when GLAZE_SOURCE_DIR is unset")

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
