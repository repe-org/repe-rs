// REPE interop fixture generator.
//
// Emits authentic REPE v1 wire frames produced by the canonical C++
// implementation (Glaze, https://github.com/stephenberry/glaze) so the Rust
// `repe` crate can pin cross-language wire compatibility against bytes it did
// not author. Each fixture is one raw `<name>.repe` frame plus an entry in
// `manifest.json` describing the expected decode.
//
// Build & run via interop/cpp/CMakeLists.txt; see interop/README.md and
// docs/interop.md for the regeneration procedure. The committed fixtures are
// generated against the pinned Glaze tag the CI job also fetches, so a drift
// check (`git diff --exit-code`) catches any future divergence.

#include <cstdint>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <string>
#include <vector>

#include "glaze/glaze.hpp"
#include "glaze/rpc/repe/buffer.hpp"
#include "glaze/rpc/repe/repe.hpp"

namespace repe = glz::repe;
namespace fs = std::filesystem;

// A representative JSON/BEVE object body: covers string, signed integer,
// double, bool, and array element types. Declared as a plain aggregate so
// Glaze's pure reflection serializes it (keys follow declaration order).
struct demo_body
{
   std::string name = "sensor-7";
   int32_t count = 42;
   double ratio = -3.5;
   bool active = true;
   std::vector<int32_t> values = {1, 2, 3};
};

// One manifest record per fixture. All numeric header fields are widened to
// uint64_t so the JSON encodes them as plain numbers (uint8_t would serialize
// as a character) and the Rust side can read them uniformly.
struct fixture_entry
{
   std::string name;
   std::string description;
   uint64_t id{};
   uint64_t notify{};
   uint64_t ec{};
   uint64_t query_format{};
   uint64_t body_format{};
   uint64_t query_length{};
   uint64_t body_length{};
   uint64_t length{};
   std::string query;
   std::string body_kind; // "json" | "beve_struct" | "beve_f64" | "utf8" | "none"
   std::string body_json; // canonical JSON of the expected body (json/beve_struct/beve_f64)
   std::string body_text; // UTF-8 message (utf8 error fixtures)
   bool encoder_parity{}; // whether Rust should assert from-scratch byte identity
};

struct manifest_doc
{
   std::string repe_version = "1";
   std::string glaze_version;
   std::string note =
      "Authentic Glaze-produced REPE v1 frames. Regenerate with interop/cpp; do not hand-edit.";
   std::vector<fixture_entry> fixtures;
};

namespace
{
   fixture_entry from_msg(std::string name, std::string description, const repe::message& msg,
                          std::string body_kind, std::string body_json, std::string body_text,
                          bool encoder_parity)
   {
      fixture_entry e;
      e.name = std::move(name);
      e.description = std::move(description);
      e.id = msg.header.id;
      e.notify = msg.header.notify;
      e.ec = static_cast<uint64_t>(msg.header.ec);
      e.query_format = static_cast<uint64_t>(msg.header.query_format);
      e.body_format = static_cast<uint64_t>(msg.header.body_format);
      e.query_length = msg.header.query_length;
      e.body_length = msg.header.body_length;
      e.length = msg.header.length;
      e.query = msg.query;
      e.body_kind = std::move(body_kind);
      e.body_json = std::move(body_json);
      e.body_text = std::move(body_text);
      e.encoder_parity = encoder_parity;
      return e;
   }

   void write_frame(const fs::path& dir, const std::string& name, const std::string& bytes)
   {
      std::ofstream f(dir / (name + ".repe"), std::ios::binary | std::ios::trunc);
      f.write(bytes.data(), static_cast<std::streamsize>(bytes.size()));
   }
}

int main(int argc, char** argv)
{
   const fs::path out = (argc > 1) ? fs::path(argv[1]) : fs::path("fixtures");
   fs::create_directories(out);

   manifest_doc manifest;
   manifest.glaze_version = (argc > 2) ? argv[2] : "v7.7.1";

   // 1. JSON request: JSON-pointer query + JSON object body.
   {
      const demo_body v{};
      const auto msg = repe::request_json(repe::user_header{.query = "/api/v1/update", .id = 7}, v);
      write_frame(out, "json_request_with_query", repe::to_buffer(msg));
      std::string body_json;
      std::ignore = glz::write_json(v, body_json);
      manifest.fixtures.push_back(
         from_msg("json_request_with_query",
                  "JSON-pointer query with a JSON object body (string, int, double, bool, array).",
                  msg, "json", body_json, "", false));
   }

   // 2. Query-only request: JSON-pointer query, no body. Glaze leaves
   //    body_format at its default (RAW_BINARY = 0) for the no-value overload.
   {
      const auto msg = repe::request_json(repe::user_header{.query = "/status", .id = 3});
      write_frame(out, "request_query_only", repe::to_buffer(msg));
      manifest.fixtures.push_back(
         from_msg("request_query_only",
                  "Query-only request, empty body (body_format stays RAW_BINARY).", msg, "none", "",
                  "", true));
   }

   // 3. JSON response: id echoed, JSON result body, ec = 0, no query.
   {
      const demo_body v{};
      repe::message msg;
      repe::response_builder rb(msg);
      rb.reset(uint64_t{7});
      std::ignore = rb.set_body<glz::opts{.format = glz::JSON}>(v);
      write_frame(out, "json_response", repe::to_buffer(msg));
      std::string body_json;
      std::ignore = glz::write_json(v, body_json);
      manifest.fixtures.push_back(from_msg(
         "json_response", "Success response: echoed id, JSON result body, empty query, ec = 0.", msg,
         "json", body_json, "", false));
   }

   // 4. BEVE request with an object body (cross-impl BEVE object parity).
   {
      const demo_body v{};
      const auto msg = repe::request_beve(repe::user_header{.query = "/api/v1/update", .id = 8}, v);
      write_frame(out, "beve_request_struct", repe::to_buffer(msg));
      std::string body_json;
      std::ignore = glz::write_json(v, body_json);
      manifest.fixtures.push_back(
         from_msg("beve_request_struct", "BEVE-encoded object body (cross-impl BEVE object parity).",
                  msg, "beve_struct", body_json, "", false));
   }

   // 5. BEVE request whose body is a numeric f64 array -- the exact shape
   //    repe-rs body_typed_slice / decode_typed_slice produce. Pins
   //    beve-rs <-> Glaze-BEVE byte identity for typed numeric arrays.
   {
      const std::vector<double> nums{1.5, -2.5, 3.25, 1000000000.0, 0.0};
      const auto msg = repe::request_beve(repe::user_header{.query = "/sensors/raw", .id = 9}, nums);
      write_frame(out, "beve_request_typed_numeric", repe::to_buffer(msg));
      std::string body_json;
      std::ignore = glz::write_json(nums, body_json);
      manifest.fixtures.push_back(
         from_msg("beve_request_typed_numeric",
                  "BEVE numeric f64 array body (beve-rs typed-slice <-> Glaze BEVE parity).", msg,
                  "beve_f64", body_json, "", true));
   }

   // 6. Notify request: notify flag set (fire-and-forget, no response expected).
   {
      const demo_body v{};
      const auto msg = repe::request_json(
         repe::user_header{.query = "/events/log", .id = 11, .notify = true}, v);
      write_frame(out, "notify_request", repe::to_buffer(msg));
      std::string body_json;
      std::ignore = glz::write_json(v, body_json);
      manifest.fixtures.push_back(from_msg(
         "notify_request", "Notify request (notify = 1) with a JSON body.", msg, "json", body_json,
         "", false));
   }

   // 7. Error response: method-not-found (ec = 6), UTF-8 message body.
   {
      repe::message msg;
      repe::response_builder rb(msg);
      rb.reset(uint64_t{7});
      const std::string message = "method not found: /missing";
      rb.set_error(static_cast<glz::error_code>(6), message);
      write_frame(out, "error_method_not_found", repe::to_buffer(msg));
      manifest.fixtures.push_back(
         from_msg("error_method_not_found",
                  "Error response, ec = 6 (method not found), UTF-8 message body.", msg, "utf8", "",
                  message, true));
   }

   // 8. Error response: invalid body (ec = 4), UTF-8 message body.
   {
      repe::message msg;
      repe::response_builder rb(msg);
      rb.reset(uint64_t{12});
      const std::string message = "invalid body: expected array of f64";
      rb.set_error(static_cast<glz::error_code>(4), message);
      write_frame(out, "error_invalid_body", repe::to_buffer(msg));
      manifest.fixtures.push_back(
         from_msg("error_invalid_body", "Error response, ec = 4 (invalid body), UTF-8 message body.",
                  msg, "utf8", "", message, true));
   }

   // 9. Minimal frame: empty query and empty body (header only).
   {
      const auto msg = repe::request_json(repe::user_header{.query = "", .id = 0});
      write_frame(out, "empty_query_empty_body", repe::to_buffer(msg));
      manifest.fixtures.push_back(
         from_msg("empty_query_empty_body", "Minimal frame: empty query, empty body (48-byte header).",
                  msg, "none", "", "", true));
   }

   std::string manifest_str;
   if (const auto ec = glz::write<glz::opts{.format = glz::JSON, .prettify = true}>(manifest,
                                                                                    manifest_str);
       ec) {
      std::cerr << "failed to serialize manifest\n";
      return 1;
   }
   {
      std::ofstream mf(out / "manifest.json", std::ios::binary | std::ios::trunc);
      mf << manifest_str << '\n';
   }

   std::cout << "wrote " << manifest.fixtures.size() << " fixtures + manifest.json to "
             << fs::absolute(out) << " (glaze " << manifest.glaze_version << ")\n";
   return 0;
}
