// REPE plugin-ABI interop host.
//
// A C++ REPE host that `dlopen`s a Rust plugin built by the `repe` crate and
// drives it through `glaze/rpc/repe/plugin.h` — the ABI both implementations
// share. Where the fixture generator pins the *wire format* against Glaze, this
// pins the *plugin ABI* against it: symbol names and signatures, the metadata
// struct's layout, the response-buffer contract, and the lifecycle.
//
// Every frame here is encoded by Glaze (`repe::request_json` / `repe::to_buffer`)
// and every response decoded by Glaze (`repe::from_buffer` / `repe::decode_message`).
// That is the point of the file: nothing hand-rolls a header, so a divergence in
// either implementation's framing shows up as a failed decode rather than as a
// byte comparison that both sides got equally wrong.
//
// Build via interop/cpp/CMakeLists.txt; see interop/README.md. Usage:
//
//   plugin_host <path-to-plugin-library>
//
// Exits non-zero on the first failed expectation, with the failures listed.

#include <dlfcn.h>

#include <cstdint>
#include <iostream>
#include <string>
#include <string_view>
#include <vector>

#include "glaze/glaze.hpp"
#include "glaze/rpc/repe/buffer.hpp"
#include "glaze/rpc/repe/plugin.h"
#include "glaze/rpc/repe/repe.hpp"

namespace repe = glz::repe;

namespace
{
   int failures = 0;

   void check(bool ok, std::string_view what)
   {
      std::cout << (ok ? "  ok   " : "  FAIL ") << what << '\n';
      if (!ok) {
         ++failures;
      }
   }

   // The five entry points, resolved by name exactly as a production host does.
   // `init` and `shutdown` are optional per plugin.h ("may be NULL"), so they are
   // resolved but not required.
   struct plugin_abi
   {
      uint32_t (*interface_version)(void){};
      const repe_plugin_data* (*info)(void){};
      repe_result (*init)(void){};
      void (*shutdown)(void){};
      repe_buffer (*call)(const char*, uint64_t){};
   };

   // Call across the ABI and copy immediately. The returned buffer is valid only
   // until this thread's next call, so a host that keeps it must copy first —
   // doing that here is not defensive coding, it is the contract.
   //
   // Returns std::nullopt for a zero-size response, which is what a notify
   // produces and which a host must read as "send nothing" rather than an error.
   std::optional<std::string> host_call(const plugin_abi& abi, std::string_view request)
   {
      const repe_buffer out = abi.call(request.data(), request.size());
      if (out.data == nullptr) {
         check(false, "response data pointer is never null");
         return std::nullopt;
      }
      // Constructing a string_view from (data, 0) is exactly what a C++ host
      // does and is undefined if data is null — the reason the plugin returns a
      // real address even when empty.
      const std::string_view view{out.data, out.size};
      if (view.empty()) {
         return std::nullopt;
      }
      return std::string{view};
   }

   // Decode a response frame with Glaze and pull out the typed body.
   template <auto Opts = glz::opts{}, class T>
   bool decode_body(const std::string& frame, T& value, std::string_view what)
   {
      repe::message msg{};
      if (repe::from_buffer(frame, msg) != glz::error_code::none) {
         check(false, std::string{what} + ": Glaze could not parse the response frame");
         return false;
      }
      if (const auto err = repe::decode_message<Opts>(value, msg)) {
         check(false, std::string{what} + ": " + *err);
         return false;
      }
      return true;
   }

   // Header fields of a response, for the cases where the *error* is the result.
   repe::message parse(const std::string& frame)
   {
      repe::message msg{};
      (void)repe::from_buffer(frame, msg);
      return msg;
   }
}

int main(int argc, char** argv)
{
   if (argc < 2) {
      std::cerr << "usage: plugin_host <path-to-plugin-library>\n";
      return 2;
   }
   const char* path = argv[1];

   void* handle = dlopen(path, RTLD_NOW | RTLD_LOCAL);
   if (!handle) {
      std::cerr << "dlopen(" << path << ") failed: " << dlerror() << '\n';
      return 2;
   }
   // Deliberately never dlclose: the plugin's response buffer is a thread-local,
   // and unloading while threads that touched it are alive leaves TLS destructor
   // addresses dangling. Documented in docs/plugins.md as a host requirement.

   plugin_abi abi{};
   abi.interface_version = reinterpret_cast<uint32_t (*)(void)>(dlsym(handle, "repe_plugin_interface_version"));
   abi.info = reinterpret_cast<const repe_plugin_data* (*)(void)>(dlsym(handle, "repe_plugin_info"));
   abi.init = reinterpret_cast<repe_result (*)(void)>(dlsym(handle, "repe_plugin_init"));
   abi.shutdown = reinterpret_cast<void (*)(void)>(dlsym(handle, "repe_plugin_shutdown"));
   abi.call = reinterpret_cast<repe_buffer (*)(const char*, uint64_t)>(dlsym(handle, "repe_plugin_call"));

   std::cout << "symbols\n";
   check(abi.interface_version != nullptr, "repe_plugin_interface_version resolves");
   check(abi.info != nullptr, "repe_plugin_info resolves");
   check(abi.call != nullptr, "repe_plugin_call resolves");
   if (!abi.interface_version || !abi.info || !abi.call) {
      return 1; // Nothing below can run without these three.
   }

   // --- version handshake, before the metadata struct is read ---------------
   std::cout << "version\n";
   check(abi.interface_version() == REPE_PLUGIN_INTERFACE_VERSION,
         "plugin reports the interface version this host was built against");
   if (abi.interface_version() != REPE_PLUGIN_INTERFACE_VERSION) {
      // Fatal, not merely recorded. `plugin.h` is explicit that the version is
      // checked *before* the struct layout is interpreted: reading a
      // `repe_plugin_data` written by a different version is what the standalone
      // version function exists to prevent, and this file is the reference for
      // the sequence.
      return 1;
   }

   // --- metadata ------------------------------------------------------------
   std::cout << "metadata\n";
   const repe_plugin_data* info = abi.info();
   check(info != nullptr, "repe_plugin_info returns a struct");
   if (!info) {
      return 1;
   }
   const std::string root = info->root_path;
   std::cout << "       name='" << info->name << "' version='" << info->version << "' root='" << root << "'\n";
   check(!root.empty() && root.front() == '/', "root_path is an absolute JSON Pointer prefix");
   check(root.size() == 1 || root.back() != '/', "root_path carries no trailing separator");

   // --- lifecycle -----------------------------------------------------------
   std::cout << "lifecycle\n";
   if (abi.init) {
      check(abi.init() == REPE_OK, "first repe_plugin_init reports REPE_OK");
      check(abi.init() == REPE_ERROR_ALREADY_INITIALIZED, "second repe_plugin_init reports ALREADY_INITIALIZED");
   }

   // --- a field read: empty body means read ---------------------------------
   std::cout << "field read\n";
   {
      const auto request = repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/gain", .id = 1}));

      // A host routes by prefix: this is how it decides the frame is ours.
      check(repe::extract_query(request).starts_with(root), "Glaze's extract_query matches the plugin's root");

      if (const auto response = host_call(abi, request)) {
         double gain = 0;
         if (decode_body(*response, gain, "read /gain")) {
            check(gain == 1.0, "read /gain returns the constructed value");
         }
         check(parse(*response).header.id == 1, "the response echoes the request id");
         check(repe::extract_query(*response) == root + "/gain", "the response echoes the request query");
      }
      else {
         check(false, "a non-notify read produces a response");
      }
   }

   // --- a field write, then a read that observes it -------------------------
   std::cout << "field write\n";
   {
      const auto write = repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/channel", .id = 2}, 6u));
      check(host_call(abi, write).has_value(), "a write is acknowledged");

      const auto read = repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/channel", .id = 3}));
      if (const auto response = host_call(abi, read)) {
         uint32_t channel = 0;
         if (decode_body(*response, channel, "read /channel")) {
            check(channel == 6, "the read observes what the write left behind");
         }
      }
   }

   // --- a method call taking one argument -----------------------------------
   std::cout << "method call\n";
   {
      const auto request =
         repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/calibrate", .id = 4}, 8.0));
      if (const auto response = host_call(abi, request)) {
         double gain = 0;
         if (decode_body(*response, gain, "/calibrate")) {
            check(gain == 4.0, "the method returns its computed result");
         }
      }
   }

   // --- a method returning Err becomes a REPE error frame -------------------
   std::cout << "method error\n";
   {
      const auto request =
         repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/calibrate", .id = 5}, -1.0));
      if (const auto response = host_call(abi, request)) {
         const auto msg = parse(*response);
         check(msg.header.ec != glz::error_code::none, "a handler Err arrives as an error frame");
         check(std::string_view{msg.body}.find("instrument fault") != std::string_view::npos,
               "the error frame carries the handler's message");
      }
   }

   // --- a zero-argument method ----------------------------------------------
   std::cout << "zero-argument method\n";
   {
      const auto request = repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/identify", .id = 6}));
      if (const auto response = host_call(abi, request)) {
         std::string identity;
         if (decode_body(*response, identity, "/identify")) {
            check(identity.starts_with("instrument fw"), "the method result decodes as a string");
         }
      }
   }

   // --- a BEVE typed numeric array field ------------------------------------
   // `#[repe(typed)]` sends the array through the bulk BEVE encoder, so this
   // crosses the boundary as BEVE while everything above is JSON.
   std::cout << "typed numeric field\n";
   {
      const auto request = repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/samples", .id = 7}));
      if (const auto response = host_call(abi, request)) {
         const auto msg = parse(*response);
         check(msg.header.body_format == repe::body_format::BEVE,
               "a #[repe(typed)] field responds with a BEVE body");
         std::vector<double> samples;
         if (decode_body<glz::opts{glz::BEVE}>(*response, samples, "/samples")) {
            check(samples.size() == 8, "Glaze decodes the typed array to its full length");
         }
      }
   }

   // --- notify: no response at all ------------------------------------------
   std::cout << "notify\n";
   {
      const auto request =
         repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/reset", .id = 8, .notify = true}));
      check(!host_call(abi, request).has_value(), "a notify answers with size 0 and a non-null pointer");
   }

   // --- an unknown method under the plugin's root ---------------------------
   std::cout << "unknown method\n";
   {
      const auto request = repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/absent", .id = 9}));
      if (const auto response = host_call(abi, request)) {
         const auto msg = parse(*response);
         check(msg.header.ec == glz::error_code::method_not_found,
               "an unknown method is method_not_found, not a dropped frame");
      }
   }

   // --- a frame Glaze would never emit --------------------------------------
   std::cout << "malformed frame\n";
   {
      const std::string garbage = "not a repe frame at all";
      if (const auto response = host_call(abi, garbage)) {
         const auto msg = parse(*response);
         check(msg.header.ec != glz::error_code::none, "a malformed frame is answered with an error");
         check(msg.header.id == 0, "the id is 0, because it could not be read");
      }
      else {
         check(false, "a malformed frame is answered, not silently dropped");
      }
   }

   // --- shutdown, last, because it is visible to everything above -----------
   std::cout << "shutdown\n";
   if (abi.shutdown) {
      abi.shutdown();
      const auto request = repe::to_buffer(repe::request_json(repe::user_header{.query = root + "/gain", .id = 10}));
      if (const auto response = host_call(abi, request)) {
         check(parse(*response).header.ec != glz::error_code::none,
               "a call after shutdown is refused with an error");
      }
      else {
         check(false, "a call after shutdown is answered, not ignored");
      }
   }

   std::cout << (failures == 0 ? "\nplugin ABI interop: PASS\n" : "\nplugin ABI interop: FAIL\n");
   return failures == 0 ? 0 : 1;
}
