// REPE plugin-ABI interop plugin.
//
// A C++ Glaze plugin, built as a shared library, for a *Rust* host to `dlopen`.
// The mirror of plugin_host.cpp: where that pins this crate's plugin exports
// against Glaze, this pins its host against Glaze's.
//
// The two halves are not redundant. A single implementation driving itself
// agrees with itself by construction — it can misread the buffer contract, the
// metadata layout, or the meaning of a zero-size response at both ends and pass.
// Only a Rust host against a C++ plugin, and a C++ host against a Rust plugin,
// pin the ABI to what `glaze/rpc/repe/plugin.h` actually says.
//
// The published surface deliberately matches examples/repe_plugin.rs field for
// field and method for method, so one host binary drives both with the same
// expectations:
//
//   cargo build --release --features plugin-host --example plugin_host
//   target/release/examples/plugin_host interop/cpp/build/libexample_plugin.so
//
// Build via interop/cpp/CMakeLists.txt; see interop/README.md.

#include <cstdint>
#include <string>
#include <string_view>

#include "glaze/rpc/repe/plugin_helper.hpp"

// A named namespace, not an anonymous one: Glaze reflects off the type, and that
// machinery needs it to have linkage.
namespace repe_interop
{
   // The path prefix this plugin claims. The registry needs it as a compile-time
   // `string_view`; `repe_plugin_data` needs a nul-terminated `const char*`. Both
   // spellings are here, tied together by the assertion, so the surface the
   // plugin serves and the root it advertises to the host cannot drift apart.
   inline constexpr std::string_view root = "/instrument";
   inline constexpr const char* root_path = "/instrument";
   static_assert(root == root_path);

   // The same object examples/repe_plugin.rs publishes: fields are readable and
   // writable state, methods are commands.
   struct instrument
   {
      double gain = 1.0;
      uint32_t channel = 0;
      std::string firmware = "1.4.2";

      std::string identify() { return "instrument fw " + firmware + " on channel " + std::to_string(channel); }

      double calibrate(double reference)
      {
         gain = reference / 2.0;
         return gain;
      }

      void reset() { gain = 1.0; }
   };
}

// Registering the methods. Glaze's aggregate reflection publishes data members
// and not member functions, so without this specialization the registry serves
// the three fields and answers `invalid_query` for both method paths.
// (`write_function_pointers` does not help: it governs how an entry already
// named in a `glz::meta` is serialized.)
//
// This is the C++ counterpart of `#[repe(methods)]` on the Rust side, and the
// asymmetry is why the Rust example needs no equivalent list: a Rust attribute
// macro attaches to the `impl` block and reads the signatures out of it, where a
// template specialization has nowhere to attach.
template <>
struct glz::meta<repe_interop::instrument>
{
   using T = repe_interop::instrument;
   static constexpr auto value =
      object(&T::gain, &T::channel, &T::firmware, &T::identify, &T::calibrate, &T::reset);
};

namespace repe_interop
{
   inline instrument device{};

   inline glz::registry<> registry = [] {
      glz::registry<> r;
      r.on<root>(device);
      return r;
   }();
}

extern "C" {

uint32_t repe_plugin_interface_version() { return REPE_PLUGIN_INTERFACE_VERSION; }

const repe_plugin_data* repe_plugin_info()
{
   // File-scope static, so the pointers outlive the library exactly as plugin.h
   // requires.
   static const repe_plugin_data info{
      .name = "interop-instrument", .version = "1.0.0", .root_path = repe_interop::root_path};
   return &info;
}

// Both lifecycle symbols are optional in this ABI. They are exported anyway,
// because a plugin that omits them exercises the host's *absent* path rather
// than its present one, and the Rust plugin already covers absent-vs-present
// from the other side.
repe_result repe_plugin_init() { return REPE_OK; }

void repe_plugin_shutdown() {}

repe_buffer repe_plugin_call(const char* request, uint64_t request_size)
{
   return glz::repe::plugin_call(repe_interop::registry, request, request_size);
}
}
