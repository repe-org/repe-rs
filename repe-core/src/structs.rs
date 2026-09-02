use crate::constants::{BodyFormat, ErrorCode};

/// Errors produced while handling struct-backed endpoints.
///
/// Non-exhaustive: match with a wildcard arm, or use [`StructError::code`] to
/// map any error onto its protocol [`ErrorCode`].
///
/// There is no `Serialize` variant. structio's writers cannot fail — a
/// [`structio::json::Write`] impl returns `()` — so encoding a response is
/// infallible and the only failures left are about the request: a path that
/// does not resolve, a body that should or should not be there, a body that
/// does not parse, and a handler that reported its own failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StructError {
    #[error("invalid path `{path}`")]
    InvalidPath { path: String },
    #[error("unexpected additional path segments at `{path}`")]
    InvalidSubpath { path: String },
    #[error("body required for `{path}`")]
    BodyExpected { path: String },
    #[error("body not allowed for `{path}`")]
    BodyUnexpected { path: String },
    /// The request body did not parse as the endpoint's type.
    ///
    /// `source` is [`structio::Error`], a `Copy` code-and-offset pair rather
    /// than an allocated message, and it carries the offending key when the
    /// reader knew one.
    #[error("could not decode body for `{path}`: {source}")]
    Decode {
        path: String,
        #[source]
        source: structio::Error,
    },
    /// The body of a multi-argument method call was the wrong *shape* — the
    /// wrong number of positional arguments, or neither an array nor an object.
    ///
    /// Distinct from [`Decode`](StructError::Decode), which is one argument
    /// failing to parse as its own type. This one is about the envelope, and it
    /// carries a message of repe's own because no parser produced it.
    #[error("invalid arguments for `{path}`: {message}")]
    Arguments { path: String, message: String },
    #[error("method `{path}` failed: {message}")]
    Execution { path: String, message: String },
}

impl StructError {
    pub fn code(&self) -> ErrorCode {
        match self {
            StructError::InvalidPath { .. } | StructError::InvalidSubpath { .. } => {
                ErrorCode::MethodNotFound
            }
            StructError::BodyExpected { .. } | StructError::BodyUnexpected { .. } => {
                ErrorCode::InvalidBody
            }
            StructError::Decode { .. } | StructError::Arguments { .. } => ErrorCode::InvalidBody,
            StructError::Execution { .. } => ErrorCode::ParseError,
        }
    }
}

/// Convenience alias for results returned by struct handlers.
pub type StructResult<T> = Result<T, StructError>;

/// Everything a served field needs to cross the wire in both of REPE's body
/// formats: readable and writable as JSON and as BEVE.
///
/// A served *field* is read by one peer and written by the other, and the frame
/// header decides the format per request rather than per type, so all four
/// halves are genuinely required of it.
///
/// The halves are separately nameable, and the crate names them separately
/// wherever only one is used: [`ServableRead`] for a body being decoded,
/// [`ServableWrite`] for a response being encoded. Asking a type that is only
/// ever sent to also be readable — or a method parameter that is only ever
/// received to also be writable — is the bound doing more than the code does.
///
/// Declare a type with structio's [`object!`](structio::object) (or
/// [`tagged_enum!`](structio::tagged_enum), or [`array!`](structio::array)) and
/// it satisfies all three by construction; there is no derive to add.
pub trait Servable: for<'de> ServableRead<'de> + ServableWrite {}
impl<T: for<'de> ServableRead<'de> + ServableWrite> Servable for T {}

/// The read half of [`Servable`]: decodable from either body format.
///
/// Carries the body's lifetime, so a type whose fields borrow out of the frame
/// satisfies it for that frame.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be decoded from a REPE body: it has no structio declaration",
    label = "no JSON/BEVE encoding for `{Self}`",
    note = "declare it with `structio::object!({Self} {{ field, .. }})` — or `tagged_enum!` / `array!` / `unit_enum!`",
    note = "`#[derive(RepeStruct)]` publishes endpoints; the structio declaration is what gives the type a wire encoding. A served type needs both."
)]
pub trait ServableRead<'de>: structio::json::Read<'de> + structio::beve::Read<'de> {}
impl<'de, T> ServableRead<'de> for T where T: structio::json::Read<'de> + structio::beve::Read<'de> {}

/// A response a client decodes into: readable in both formats, and owning its
/// own contents.
///
/// [`ServableRead`] carries the frame's lifetime, which is what lets a *server*
/// borrow out of the body it was handed. A client cannot: it decodes out of a
/// response message it owns locally and then returns the value, so the value
/// has to outlive the frame it came from. `for<'de>` says exactly that, and
/// `Default` is there because structio reads *into* an existing value.
///
/// The bound is both formats rather than the one the request used, because a
/// server answers in the format the frame asked for and a client that names a
/// result type does not get to assume which one came back.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be decoded from a REPE response",
    label = "not decodable as an owned JSON/BEVE value",
    note = "declare it with `structio::object!({Self} {{ field, .. }})` — or `tagged_enum!` / `array!` / `unit_enum!`",
    note = "a type whose fields borrow (`&'de str`) satisfies `ServableRead` but not this: a decoded response outlives the frame, so borrowing fields must be owned (`String`)"
)]
pub trait ServableOwned: for<'de> ServableRead<'de> + Default {}
impl<T: for<'de> ServableRead<'de> + Default> ServableOwned for T {}

/// The write half of [`Servable`]: encodable into either body format.
///
/// Both halves, because the frame header picks the format per request and a
/// response is encoded in whichever one the request asked for.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be encoded into a REPE body: it has no structio declaration",
    label = "no JSON/BEVE encoding for `{Self}`",
    note = "declare it with `structio::object!({Self} {{ field, .. }})` — or `tagged_enum!` / `array!` / `unit_enum!`",
    note = "`#[derive(RepeStruct)]` publishes endpoints; the structio declaration is what gives the type a wire encoding. A served type needs both."
)]
pub trait ServableWrite: structio::json::Write + structio::beve::Write {}
impl<T: structio::json::Write + structio::beve::Write + ?Sized> ServableWrite for T {}

/// The request body handed to a [`RepeStruct`], as the bytes that arrived plus
/// the format the frame header declared.
///
/// This is the mirror of [`ResponseBody`], and it is what replaced the
/// `Option<serde_json::Value>` the trait used to take. A body is now parsed
/// **once, into the live member it is destined for** — Glaze's `read_params` —
/// rather than into a tree that is then walked and re-parsed. Nothing here
/// allocates until a reader does, and a request that no endpoint claims never
/// parses at all.
///
/// The bytes are borrowed from the frame, so a `RequestBody` cannot outlive it.
/// That is deliberate: it is what lets a `&'de str` field borrow from the
/// request instead of copying out of it.
#[derive(Debug, Clone, Copy)]
pub struct RequestBody<'a> {
    bytes: &'a [u8],
    format: BodyFormat,
}

impl<'a> RequestBody<'a> {
    /// Wrap the body bytes of a frame, under the format its header declared.
    pub fn new(bytes: &'a [u8], format: BodyFormat) -> Self {
        Self { bytes, format }
    }

    /// The raw body bytes, unparsed.
    ///
    /// For an endpoint that forwards a body onward without looking inside it —
    /// a proxy, a plugin boundary — and for [`BodyFormat::RawBinary`], where
    /// there is nothing to parse.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The body format the frame header declared.
    pub fn format(&self) -> BodyFormat {
        self.format
    }

    /// Whether the body is the literal `null`.
    ///
    /// Asked by the paths that distinguish "no body" from "a body saying
    /// nothing" — an optional child refusing a presence write, most of all.
    /// Cheap in both formats: one tag byte in BEVE, a four-byte compare in
    /// JSON. JSON whitespace is ASCII, so the compare needs no UTF-8 pass over
    /// a body that may be megabytes long.
    pub fn is_null(&self) -> bool {
        match self.format {
            BodyFormat::Beve => structio::beve::Reader::new(self.bytes)
                .try_null()
                .unwrap_or(false),
            _ => self.bytes.trim_ascii() == b"null",
        }
    }

    /// Parse the body into `value`, in place.
    ///
    /// `path` names the endpoint for the error message and is only read when
    /// parsing fails.
    ///
    /// Reading *into* rather than returning a new value is what makes this the
    /// live-member read: a `Vec` or `String` already in the destination keeps
    /// its allocation, and a field absent from the body keeps the value it had.
    /// That last part is a **merge**, and it is the behaviour a whole-object
    /// write wants — a client updating one key of a wide object should not
    /// blank the rest.
    ///
    /// An endpoint that wants some members to be mandatory marks them
    /// `#[required]` in its structio declaration, which is per field and unions
    /// with whatever policy is in force.
    pub fn read_into<T>(&self, path: &str, value: &mut T) -> StructResult<()>
    where
        T: ServableRead<'a>,
    {
        self.read_into_with::<structio::Standard, T>(path, value)
    }

    /// [`read_into`](Self::read_into) under an explicit structio
    /// [read policy](structio::Options).
    ///
    /// For a policy that changes what the *body* may contain:
    /// [`AllowComments`](structio::AllowComments) for a body written by hand,
    /// [`SkipUnknown`](structio::SkipUnknown) for one from a newer peer.
    ///
    /// Not for making members mandatory. [`RequireKeys`](structio::RequireKeys)
    /// demands *every* declared member, and an `Option<T>` is not exempt — the
    /// test is whether the key is present, not what it holds, so `null`
    /// satisfies it and absence does not, and a struct with one optional member
    /// cannot be read under it at all. Mark the mandatory members `#[required]`
    /// instead.
    pub fn read_into_with<O, T>(&self, path: &str, value: &mut T) -> StructResult<()>
    where
        O: structio::Options,
        T: ServableRead<'a>,
    {
        self.read_raw_with::<O, T>(value)
            .map_err(|source| StructError::Decode {
                path: path.to_string(),
                source,
            })
    }

    /// [`read_into`](Self::read_into) without the path, for a caller that
    /// builds the error label only when there is an error to label.
    fn read_into_raw<T>(&self, value: &mut T) -> Result<(), structio::Error>
    where
        T: ServableRead<'a>,
    {
        self.read_raw_with::<structio::Standard, T>(value)
    }

    fn read_raw_with<O, T>(&self, value: &mut T) -> Result<(), structio::Error>
    where
        O: structio::Options,
        T: ServableRead<'a>,
    {
        match self.format {
            BodyFormat::Beve => structio::beve::read_into_with::<O, T>(value, self.bytes),
            _ => match core::str::from_utf8(self.bytes) {
                Ok(text) => structio::json::read_into_with::<O, T>(value, text),
                Err(err) => Err(structio::Error::new(
                    structio::ErrorCode::InvalidUtf8,
                    err.valid_up_to(),
                )),
            },
        }
    }

    /// Parse the body as a fresh `T`.
    ///
    /// [`read_into`](Self::read_into) is the one to reach for when a
    /// destination already exists; this is for the case where it does not, such
    /// as a method's parameter struct. The `Default` bound is what structio's
    /// read-into model costs here: the value has to exist before it can be read
    /// into.
    pub fn read<T>(&self, path: &str) -> StructResult<T>
    where
        T: ServableRead<'a> + Default,
    {
        let mut value = T::default();
        self.read_into(path, &mut value)?;
        Ok(value)
    }
}

/// Trait implemented by structs that can be exposed directly through the REPE
/// router.
///
/// The implementation interprets the JSON Pointer path segments and either
/// writes a response into `out` (for reads) or mutates the struct (for writes).
///
/// There is one dispatch method rather than two. The `Value`-returning
/// `repe_handle` is gone with `serde_json::Value` itself: a read now serializes
/// the live field straight into the outgoing frame, and a write parses the
/// request bytes straight into it, so there is no tree for a second method to
/// hand back.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be served over REPE: it does not implement `RepeStruct`",
    label = "not a REPE-servable type",
    note = "derive it with `#[derive(RepeStruct)]`",
    note = "a field's own type needs a structio declaration — `structio::object!(Field {{ .. }})` — not a derive",
    note = "a type in a crate that depends only on `repe-core` can derive this too; `repe` re-exports the same trait"
)]
pub trait RepeStruct: Send + Sync {
    /// Handle a read or write against this struct, writing any response into
    /// `out`.
    ///
    /// * `segments` — JSON Pointer path split into unescaped segments.
    /// * `body` — `Some(..)` when the frame carried a body, `None` for a bare
    ///   read.
    /// * `out` — the response body under construction. Write exactly one
    ///   top-level value into it: a value, a [`null`](ResponseBody::write_null),
    ///   or an [`object`](ResponseBody::object). A write endpoint answers with
    ///   `null`.
    fn repe_handle_into(
        &mut self,
        segments: &[&str],
        body: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> StructResult<()>;

    /// Serve a request through a **shared** borrow, or decline it.
    ///
    /// The router tries this first for every request against a struct
    /// registered behind a lock that has a shared mode, which is what keeps a
    /// `/version` read from queueing behind a half-second method call on the
    /// same object.
    ///
    /// A body-carrying frame is offered here too, and that is deliberate: REPE
    /// separates read from write at the *frame* level, but a `&self` method
    /// taking arguments is a call, not a mutation. Deciding from the frame
    /// alone put a long-running `&self` call behind the write guard and stalled
    /// every read of the object for as long as it ran. The receiver is known
    /// where these arms are generated, so it is the receiver that decides.
    ///
    /// Return `None` for any path this borrow cannot serve: a field write, a
    /// `&mut self` method, a nested field whose child declined. The router then
    /// retakes the lock exclusively and dispatches through
    /// [`repe_handle_into`](Self::repe_handle_into), so declining costs
    /// correctness nothing — it is the default, and a hand-written impl that
    /// never overrides this behaves exactly as it did before the path existed.
    ///
    /// Two obligations come with overriding it:
    ///
    /// * **Answer identically.** Whatever this returns for a path is what the
    ///   client sees; the exclusive path is not consulted afterwards. Decline
    ///   rather than approximate.
    /// * **Write nothing when declining.** A `None` return must leave `out` as
    ///   it was found, so the exclusive attempt starts from an empty body.
    ///   [`ObjectBody::entry_try_with`] exists to make that mechanical when the
    ///   decline surfaces partway through an object: it rewinds the whole
    ///   object, so propagating its `None` is all a caller has to do.
    ///
    /// The third obligation this trait used to carry — leave `body` alone when
    /// declining — is gone. [`RequestBody`] is `Copy` and borrows the frame
    /// rather than owning a parsed tree, so the exclusive retry re-dispatches
    /// the same body with no clone and nothing to take. That is the second
    /// thing removing `Value` bought, after the allocation.
    ///
    /// A derived impl settles the whole-object listing before it writes or
    /// invokes anything, through
    /// [`repe_listing_declines`](Self::repe_listing_declines) — because a
    /// decline discovered later would leave the entries before it run twice,
    /// once here and once on the exclusive retry, and a rewind cannot undo a
    /// call. A hand-written impl that invokes anything before it can decide owes
    /// the same care.
    fn repe_shared_into(
        &self,
        segments: &[&str],
        body: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> Option<StructResult<()>> {
        let _ = (segments, body, out);
        None
    }

    /// Whether a shared whole-object listing of this struct declines — asked
    /// **before** the listing writes or invokes anything.
    ///
    /// A listing is the one read that composes many others, so a decline
    /// discovered partway through is not recoverable: the exclusive retry
    /// re-runs the entries before it, and an entry that was *invoked* rather
    /// than serialized runs a second time. Rewinding the response buffer undoes
    /// the bytes; it cannot undo a call.
    ///
    /// So the answer has to be available at the top, and it has to cover the
    /// whole subtree — a parent's listing composes its children's, so a child
    /// that declines takes the parent's listing with it. That is why this is on
    /// [`RepeStruct`] rather than only on
    /// [`RepeMethods::REPE_LISTING_NEEDS_EXCLUSIVE`], which answers the same
    /// question about one method table's accessors and nothing beneath them. A
    /// derived impl returns that const OR'd with every `#[repe(nested)]` child's
    /// answer, so the whole tree is settled before the first byte is written.
    ///
    /// The default is `true`, which is the accurate answer for the default
    /// [`repe_shared_into`](Self::repe_shared_into): it declines every path,
    /// listings included. A hand-written impl that overrides `repe_shared_into`
    /// to serve a listing should override this too, or every derived struct
    /// nesting it gives up its own shared listing.
    ///
    /// **Overriding it is a promise**, and it is the promise the invariant rests
    /// on: returning `false` asserts that `repe_shared_into(&[], None, ..)`
    /// answers `Some(..)`. Breaking it still yields a correct response — the
    /// listing rewinds and the exclusive path retries — but anything the shared
    /// attempt had already invoked runs twice.
    fn repe_listing_declines(&self) -> bool {
        true
    }

    /// Whether any path can answer a **body-carrying** frame through
    /// [`repe_shared_into`](Self::repe_shared_into).
    ///
    /// The router asks before it takes the read lock. REPE puts a body on a
    /// write *and* on a call with arguments, so a body alone cannot say which
    /// one arrived — but a struct whose every write needs `&mut self` knows the
    /// answer for all of them in advance, and `false` lets the router skip the
    /// lock, the walk and the certain decline.
    ///
    /// This is a hint, and it cannot cost correctness. Everything the shared
    /// borrow answers with a body, the exclusive path answers identically: a
    /// `&self` method is callable through either, and every refusal
    /// ([`BodyUnexpected`](StructError::BodyUnexpected) on a read-only endpoint,
    /// [`InvalidPath`](StructError::InvalidPath) on an unknown one) is generated
    /// from the same shape on both sides. `false` on an impl that could have
    /// served one costs concurrency for that frame and nothing else.
    ///
    /// The default is `true`, the accurate answer for a hand-written
    /// `repe_shared_into` this crate cannot see inside. A derived impl narrows
    /// it to `false` when nothing it generates can answer a body: no `&self`
    /// method taking arguments, no read-only endpoint whose refusal is servable
    /// shared, and no `#[repe(nested)]` child that has either.
    const REPE_SHARED_SERVES_BODIES: bool = true;
}

/// A **conditionally-present** child: a component that is not in every build, a
/// subsystem that is not configured, a resource that failed to open.
///
/// `Option` is a foreign type and [`RepeStruct`] is this crate's trait, so a
/// host cannot write this impl itself — its choices without one are a newtype
/// per crate or one impl per concrete `Option<Child>`. Carrying it here is what
/// lets `#[repe(nested)]` be put on an `Option<T>` field directly, which is
/// where a host reaches for it first.
///
/// Present forwards everything to the inner value. Absent answers:
///
/// * a **read at the child's own path** with `null`, so a whole-object listing
///   of the parent shows the key with nothing behind it rather than failing;
/// * a **write, or any subpath** with [`StructError::InvalidPath`], because a
///   silent no-op against a live resource is worse than an error — a client
///   that configured an absent component would otherwise be told it succeeded;
/// * [`repe_listing_declines`](RepeStruct::repe_listing_declines) with `false`
///   when absent, since a `null` needs no exclusive borrow, and with the inner
///   value's own answer when present.
///
/// **Presence is the host's, not the client's.** A `null` written at the child's
/// own path is refused with [`StructError::BodyUnexpected`] whether the child is
/// there or not, so the one value this path can read back is the one value it
/// will not take. Removing a child would otherwise be a door that only opens one
/// way, since creating one is the write an absent child already refuses. A
/// whole-object write at the *parent* replaces the field, `null` included; that
/// is a replace of the parent rather than a write to this path.
///
/// This matches what Glaze publishes for an unmapped optional member, so a
/// port's wire shape does not change with the language.
impl<T: RepeStruct> RepeStruct for Option<T> {
    fn repe_handle_into(
        &mut self,
        segments: &[&str],
        body: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> StructResult<()> {
        refuse_presence_write(segments, body)?;
        match self {
            Some(inner) => inner.repe_handle_into(segments, body, out),
            None => {
                absent_child(segments, body.is_some())?;
                out.write_null();
                Ok(())
            }
        }
    }

    fn repe_shared_into(
        &self,
        segments: &[&str],
        body: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> Option<StructResult<()>> {
        // Refusing touches nothing, so it needs no exclusive borrow — and it has
        // to be decided here rather than left to the retry, or a present child
        // would forward the `null` to its inner value under both guards.
        if let Err(err) = refuse_presence_write(segments, body) {
            return Some(Err(err));
        }
        match self {
            Some(inner) => inner.repe_shared_into(segments, body, out),
            // Absent is a constant: no state to borrow exclusively, so every
            // answer above is servable here.
            None => Some(absent_child(segments, body.is_some()).map(|()| out.write_null())),
        }
    }

    fn repe_listing_declines(&self) -> bool {
        match self {
            Some(inner) => inner.repe_listing_declines(),
            None => false,
        }
    }

    /// A present child answers for itself. An absent one refuses every write
    /// and every sub-path, and refuses them identically on both borrows, so it
    /// contributes nothing the exclusive path would not also say.
    const REPE_SHARED_SERVES_BODIES: bool = T::REPE_SHARED_SERVES_BODIES;
}

/// Refuse a `null` written at an optional child's **own** path, present or
/// absent.
///
/// `null` is what that path *reads* when the child is absent, so a client will
/// try writing it back, and without this it lands on whichever branch happens to
/// be live: a present child forwards it to its inner value and answers with that
/// type's parse complaint, an absent one answers `InvalidPath`. Neither says
/// what is actually true, which is that presence is not settable here.
///
/// It is refused rather than honoured because honouring it would open a one-way
/// door: `null` could remove a child, and nothing could put it back — creating
/// one is the write an absent child already refuses, for the reason that a
/// client configuring something that is not there must not be told it worked.
/// Presence belongs to the host on both sides of that. A whole-object write at
/// the *parent* still replaces the field, `null` included; that is a replace of
/// the parent, not a write to this path.
fn refuse_presence_write(segments: &[&str], body: Option<RequestBody<'_>>) -> StructResult<()> {
    if segments.is_empty() && body.is_some_and(|b| b.is_null()) {
        return Err(StructError::BodyUnexpected {
            path: path_from_segments(segments),
        });
    }
    Ok(())
}

/// The absent half of [`RepeStruct for Option<T>`](RepeStruct#impl-RepeStruct-for-Option<T>):
/// `Ok(())` for the one request an absent child can answer, and
/// [`StructError::InvalidPath`] for the rest.
///
/// The path is relative to the child, so a parent's
/// [`prepend_path`] names the field the client actually asked for.
fn absent_child(segments: &[&str], has_body: bool) -> StructResult<()> {
    if segments.is_empty() && !has_body {
        Ok(())
    } else {
        Err(StructError::InvalidPath {
            path: path_from_segments(segments),
        })
    }
}

/// Method table for a struct whose methods are declared with `#[repe::methods]`
/// on an inherent `impl` block.
///
/// Generated by the attribute macro, which sees every signature in the block and
/// so cannot fall behind the type the way a hand-maintained
/// `#[repe(methods(..))]` list can. `#[derive(RepeStruct)]` dispatches path
/// segments that match no field through this table when the struct carries the
/// `#[repe(methods)]` marker.
///
/// Implementing this by hand is supported but unusual; the macro exists so the
/// signatures stay derived from the `impl` block.
#[diagnostic::on_unimplemented(
    message = "`{Self}` declares `#[repe(methods)]` but its impl block is not annotated",
    label = "no method table for `{Self}`",
    note = "annotate the inherent `impl {Self}` block with `#[repe::methods]` (or `#[repe_core::methods]` when only `repe-core` is a dependency), or drop `#[repe(methods)]` from the struct"
)]
pub trait RepeMethods: MethodsDeclared {
    /// `(endpoint, signature)` pairs published in the whole-struct listing.
    const REPE_METHOD_SIGNATURES: &'static [(&'static str, &'static str)];

    /// Endpoints in this table that are **field-shaped**: served by a
    /// getter/setter pair (`#[repe(get = "...")]` / `#[repe(set = "...")]`)
    /// rather than by an argument list.
    ///
    /// They are deliberately absent from
    /// [`REPE_METHOD_SIGNATURES`](Self::REPE_METHOD_SIGNATURES), because they
    /// have no signature to publish: a client reads and writes one the way it
    /// reads and writes a field, so the whole-struct listing shows each one's
    /// current value rather than a signature string. The derived listing reads
    /// those values back through [`repe_call_into`](Self::repe_call_into), or
    /// through [`repe_call_shared_into`](Self::repe_call_shared_into) when it is
    /// being served under a shared borrow.
    ///
    /// Defaults to empty, so a hand-written impl need not mention it. One
    /// invariant comes with listing a name here, and the derived listing relies
    /// on it: [`repe_call_into`](Self::repe_call_into) must answer
    /// `segments = &[name], body = None` with the endpoint's value. A name the
    /// table cannot dispatch turns every whole-struct read into that name's
    /// error. `#[repe::methods]` upholds it by construction, because it emits
    /// this list from the getters it generated arms for. The shared-borrow
    /// listing asks the same question of `repe_call_shared_into`, where a `None`
    /// is not a failure — it declines the whole listing to the exclusive path.
    const REPE_ACCESSOR_ENDPOINTS: &'static [&'static str] = &[];

    /// `true` only on the placeholder table `#[repe::methods]` emits when the
    /// block itself failed to parse.
    ///
    /// That table publishes nothing, so every compile-time check against it
    /// would fire and bury the real error under a second, misleading one. This
    /// says "these tables are not the truth" so those checks can stand down —
    /// which emptiness alone cannot say, because a block that compiles and
    /// publishes nothing is empty too, and its checks must still run.
    #[doc(hidden)]
    const REPE_TABLE_RECOVERED: bool = false;

    /// Whether the whole-struct listing must be served exclusively because some
    /// entry in it can only be read through `&mut self`.
    ///
    /// This is the one question the shared listing has to settle **before** it
    /// runs anything, and it is why it is a `const` rather than a check per
    /// entry. A listing is the one read that composes many others; a decline
    /// discovered partway through leaves the entries before it already invoked,
    /// and the exclusive retry invokes them again. A `&self` getter over a read
    /// counter would report the second call.
    ///
    /// Only the getter half of a field-shaped endpoint can force it. Fields
    /// serialize, published signatures are string literals, and a nested child
    /// declines before writing anything of its own — none of those is a *call*.
    /// So this is `true` exactly when some accessor's getter takes `&mut self`.
    ///
    /// The default is the conservative one: any accessor at all forces the
    /// exclusive path, which is what a hand-written table gets for free.
    /// `#[repe::methods]` overrides it with the answer it computed from the
    /// receivers it has already seen, so a struct whose computed values are pure
    /// reads keeps its shared listing — and so does every ancestor that nests
    /// it, since a child's decline propagates.
    ///
    /// **Overriding it is a promise.** Setting it to `false` asserts that
    /// [`repe_call_shared_into`](Self::repe_call_shared_into) answers
    /// `segments = &[name], body = None` with `Some(..)` for every name in
    /// [`REPE_ACCESSOR_ENDPOINTS`](Self::REPE_ACCESSOR_ENDPOINTS). A table that
    /// breaks that promise still produces a correct response — the listing
    /// rewinds and retries exclusively — but the getters that ran before the
    /// decline run a second time, which is the double invocation this const
    /// exists to rule out.
    const REPE_LISTING_NEEDS_EXCLUSIVE: bool = !Self::REPE_ACCESSOR_ENDPOINTS.is_empty();

    /// Whether any endpoint in this table can answer a body-carrying frame
    /// through [`repe_call_shared_into`](Self::repe_call_shared_into).
    ///
    /// The table half of
    /// [`RepeStruct::REPE_SHARED_SERVES_BODIES`], which ORs this with what the
    /// struct's own fields and children say. Three shapes here can answer one: a
    /// `&self` method that takes arguments, a `&self` setter, and a read-only
    /// accessor, whose refusal needs no exclusive borrow to give.
    ///
    /// The default is `true`, which is never wrong — only, for a table that
    /// serves no body, slower than it needs to be.
    const REPE_SHARED_SERVES_BODIES: bool = true;

    /// Invoke the method named by `segments[0]`, writing its result into `out`.
    ///
    /// `segments` is the remaining path relative to the struct root, so error
    /// paths match the ones [`RepeStruct::repe_handle_into`] produces for
    /// fields. Returns [`StructError::InvalidPath`] when no method matches.
    fn repe_call_into(
        &mut self,
        segments: &[&str],
        body: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> StructResult<()>;

    /// Shared-borrow counterpart of [`repe_call_into`](Self::repe_call_into),
    /// mirroring [`RepeStruct::repe_shared_into`] and carrying the same two
    /// obligations: answer identically, and decline without writing.
    ///
    /// `#[repe::methods]` serves every `&self` method here — including one
    /// taking arguments, which is a call rather than a mutation however the
    /// frame is shaped — along with the getter half of a `&self` accessor, and
    /// declines everything else.
    fn repe_call_shared_into(
        &self,
        segments: &[&str],
        body: Option<RequestBody<'_>>,
        out: &mut ResponseBody<'_>,
    ) -> Option<StructResult<()>> {
        let _ = (segments, body, out);
        None
    }
}

/// Marker emitted by `#[derive(RepeStruct)]` when the struct carries the
/// `#[repe(methods)]` attribute.
///
/// A derive macro cannot see `impl` blocks, so the two halves of the
/// `#[repe::methods]` surface are tied together by a compile-time handshake.
/// This marker is a supertrait of [`RepeMethods`], so the table the attribute
/// macro generates simply cannot be implemented for a struct that did not
/// declare it — no assertion to emit, and none to forget. Missing either half is
/// a compile error naming the attribute that is absent, rather than a method
/// that quietly never reaches the wire.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has an annotated `methods` impl block but does not declare it",
    label = "`{Self}` is missing `#[repe(methods)]`",
    note = "add `#[repe(methods)]` next to `#[derive(RepeStruct)]` on `{Self}` so the derived router dispatches to the impl block"
)]
pub trait MethodsDeclared {}

/// Build a path string from segments for error messages.
pub fn path_from_segments(segments: &[&str]) -> String {
    let mut s = String::new();
    for segment in segments {
        s.push('/');
        s.push_str(segment);
    }
    s
}

/// Prefix error paths with an additional segment when bubbling errors from nested structs.
pub fn prepend_path(mut err: StructError, prefix: &str) -> StructError {
    let with_prefix = |path: String| {
        if prefix.is_empty() {
            path
        } else if path.is_empty() {
            format!("/{}", prefix)
        } else if path.starts_with('/') {
            format!("/{prefix}{path}")
        } else {
            format!("/{prefix}/{path}")
        }
    };

    match &mut err {
        StructError::InvalidPath { path }
        | StructError::InvalidSubpath { path }
        | StructError::BodyExpected { path }
        | StructError::BodyUnexpected { path }
        | StructError::Decode { path, .. }
        | StructError::Arguments { path, .. }
        | StructError::Execution { path, .. } => {
            *path = with_prefix(std::mem::take(path));
        }
    }
    err
}

/// Response body under construction for a [`RepeStruct`] read.
///
/// Handed to [`RepeStruct::repe_handle_into`] so a read serializes the live
/// field straight into the outgoing frame buffer, with no intermediate tree —
/// which is what a client polling a wide object at kilohertz used to pay for.
///
/// The body starts out empty, encoding in the format the request asked for.
/// [`new`](Self::new) defaults that to [`BodyFormat::Json`];
/// [`with_format`](Self::with_format) is what a server uses to answer a BEVE
/// request in BEVE. The write that fills the body settles the format it
/// actually reports, which is normally the requested one and is always
/// [`BodyFormat::Beve`] for [`write_typed_slice`](Self::write_typed_slice).
/// Exactly one top-level write is expected — a value, a null, or an
/// [`object`](Self::object).
///
/// A whole-object [`object`](Self::object) listing is JSON whatever the request
/// asked for: a BEVE object carries its member count in its header, and a
/// listing that lets entries decline does not know the count until it is done.
///
/// Writing itself cannot fail: a [`structio::json::Write`] impl returns `()`.
/// What can still fail is the *handler* around it — a nested child that
/// declines, a method that errors — so [`object`](Self::object) entries that run
/// arbitrary code keep their `Result`, and rewind the buffer to where the write
/// began, so a caller that recovers from a [`StructError`] never ships
/// half-serialized bytes.
pub struct ResponseBody<'a> {
    buf: &'a mut Vec<u8>,
    /// The format the request asked to be answered in. Only ever
    /// [`BodyFormat::Json`] or [`BodyFormat::Beve`]; a request in one of the
    /// other two is answered in JSON.
    requested: BodyFormat,
    format: BodyFormat,
    /// True while this body is a value *inside* an enclosing object, where the
    /// frame's format is already settled and a member cannot change it.
    ///
    /// It gates exactly one thing: [`write_typed_slice`](Self::write_typed_slice).
    /// A `#[repe(typed)]` field read *on its own* answers in BEVE whatever the
    /// request asked for, that being what the attribute means. The same field
    /// listed *inside* an object cannot do that without corrupting the frame, so
    /// there it writes through [`write`](Self::write) and comes out as whatever
    /// the enclosing object is — a BEVE typed array in a BEVE listing, a JSON
    /// array in a JSON one.
    ///
    /// Every other write follows `requested`, which an enclosing object sets to
    /// its own format, so nothing else needs to consult this.
    nested: bool,
}

impl<'a> ResponseBody<'a> {
    /// Wrap an output buffer, answering in JSON. `buf` is normally empty; writes
    /// append to it.
    pub fn new(buf: &'a mut Vec<u8>) -> Self {
        Self::with_format(buf, BodyFormat::Json)
    }

    /// Wrap an output buffer, answering in the format the request declared.
    ///
    /// [`BodyFormat::RawBinary`] and [`BodyFormat::Utf8`] describe bodies with
    /// no value structure to mirror, so a request in either is answered in
    /// JSON.
    pub fn with_format(buf: &'a mut Vec<u8>, requested: BodyFormat) -> Self {
        Self {
            buf,
            requested: match requested {
                BodyFormat::Beve => BodyFormat::Beve,
                _ => BodyFormat::Json,
            },
            format: BodyFormat::Json,
            nested: false,
        }
    }

    /// The body format settled by the write that filled this body.
    pub fn format(&self) -> BodyFormat {
        self.format
    }

    /// Serialize `value` as the whole body, in the requested format.
    ///
    /// Inside an enclosing object the frame is already committed to JSON, and
    /// this writes JSON regardless.
    pub fn write<T>(&mut self, value: &T)
    where
        T: ServableWrite + ?Sized,
    {
        if self.requested == BodyFormat::Beve {
            structio::beve::append(value, self.buf);
            self.format = BodyFormat::Beve;
        } else {
            structio::append(value, self.buf);
        }
    }

    /// Write a `null` body — the response to a write, or to a read that has
    /// nothing to return.
    pub fn write_null(&mut self) {
        if self.requested == BodyFormat::Beve {
            structio::beve::append(&(), self.buf);
            self.format = BodyFormat::Beve;
        } else {
            self.buf.extend_from_slice(b"null");
        }
    }

    /// Write `slice` as a BEVE typed numeric array and set the body format to
    /// [`BodyFormat::Beve`].
    ///
    /// The same bulk encode `repe::message::MessageBuilder::body_typed_slice`
    /// produces — one `copy_nonoverlapping` on little-endian targets rather than
    /// a per-element walk, and byte-identical to what Glaze emits for the same
    /// array. This is what `#[repe(typed)]` routes a numeric field to. (Named
    /// rather than linked: the builder lives in `repe`, which this crate
    /// deliberately does not depend on.)
    ///
    /// Inside an enclosing object — the whole-struct listing — the frame is
    /// already committed to JSON, so the slice is written as a JSON array
    /// instead. A field only reaches the typed encoding when it is read on its
    /// own.
    ///
    /// This used to sit behind a `typed` feature, because the encoder arrived
    /// with `beve` and six transitive packages behind it and a crate that only
    /// *declares* a served type should not link one to say so. structio has no
    /// dependencies, so there is nothing left to gate and the feature is gone.
    pub fn write_typed_slice<T>(&mut self, slice: &[T])
    where
        T: structio::beve::NumericBytes + ServableWrite,
    {
        if self.nested {
            self.write(slice);
            return;
        }
        structio::beve::append(slice, self.buf);
        self.format = BodyFormat::Beve;
    }

    /// Begin an object body of exactly `entries` members, written key by key.
    ///
    /// Used by the whole-struct listing so a wide object is emitted in one pass.
    ///
    /// `entries` is what lets the object be BEVE as readily as JSON: a BEVE
    /// object carries its member count in its header, before any member, so a
    /// listing that discovered its count by finishing could only ever have been
    /// JSON. A derived listing knows the count when it is generated, and an
    /// entry that fails or declines rewinds the whole object rather than
    /// emitting a short one, so the declared count is never wrong.
    ///
    /// Under JSON the count is unused. Under BEVE, writing a number of entries
    /// other than `entries` produces a corrupt document; a debug build asserts
    /// at [`finish`](ObjectBody::finish) rather than letting it reach a peer.
    pub fn object(&mut self, entries: usize) -> ObjectBody<'_> {
        let start = self.buf.len();
        let format = if self.nested {
            // Already inside an object, so the frame's format is settled and
            // this one follows it.
            self.format
        } else {
            self.format = self.requested;
            self.requested
        };
        match format {
            BodyFormat::Beve => {
                let mut w = beve_writer(self.buf);
                w.push(structio::beve::header::OBJECT);
                w.size(entries as u64);
                *self.buf = w.into_vec();
            }
            _ => self.buf.push(b'{'),
        }
        ObjectBody {
            buf: self.buf,
            start,
            format,
            first: true,
            declared: entries,
            written: 0,
        }
    }
}

/// A JSON object body being written key by key. Created by
/// [`ResponseBody::object`]; call [`finish`](Self::finish) to close it.
///
/// An entry that fails — or that [`entry_try_with`](Self::entry_try_with)
/// declines — rewinds the buffer past the opening brace, so a partly written
/// object is never left behind for a caller that recovers from it.
///
/// Those are the only two exits that rewind. There is no `Drop` impl, so
/// abandoning an object after a successful [`entry`](Self::entry) — an early
/// return, a panic — leaves the open brace and its entries in the buffer.
/// Generated code never does that: every listing either runs to
/// [`finish`](Self::finish) or leaves through an arm that has already rewound.
/// A hand-written listing owes the same.
pub struct ObjectBody<'a> {
    buf: &'a mut Vec<u8>,
    /// Buffer length before the object's header, to rewind to on failure.
    start: usize,
    /// The format this object and everything nested inside it is written in.
    format: BodyFormat,
    first: bool,
    declared: usize,
    written: usize,
}

/// A BEVE writer appending to `buf`, keeping what it already holds.
///
/// `Writer::appending` owns its buffer, so this moves the `Vec` out and the
/// caller moves it back with `into_vec`. Both are three-word moves; nothing is
/// copied and the allocation is preserved.
fn beve_writer(buf: &mut Vec<u8>) -> structio::beve::Writer<'static, structio::Standard> {
    structio::beve::Writer::appending(std::mem::take(buf))
}

impl ObjectBody<'_> {
    /// Write one member's key and open its value position.
    fn key(&mut self, key: &str) {
        self.written += 1;
        match self.format {
            // A BEVE key is a bare length-prefixed string: no header byte, no
            // separator, and no comma between members, the count in the header
            // having said how many to expect.
            BodyFormat::Beve => {
                let mut w = beve_writer(self.buf);
                w.write_str_body(key);
                *self.buf = w.into_vec();
            }
            _ => {
                if self.first {
                    self.first = false;
                } else {
                    self.buf.push(b',');
                }
                // `str`'s JSON encoding is the quoted, escaped form, which is
                // exactly a key. Going through structio rather than quoting by
                // hand is what keeps escaping in one place.
                structio::append(key, self.buf);
                self.buf.push(b':');
            }
        }
    }

    /// A body for one member's value, in this object's format.
    fn value_body(&mut self) -> ResponseBody<'_> {
        ResponseBody {
            buf: self.buf,
            requested: self.format,
            format: self.format,
            nested: true,
        }
    }

    /// Write one `key: value` entry.
    pub fn entry<T>(&mut self, key: &str, value: &T)
    where
        T: ServableWrite + ?Sized,
    {
        self.key(key);
        self.value_body().write(value);
    }

    /// Write one `key: <value>` entry whose value is produced by `f`, for a
    /// nested [`RepeStruct`] that fills the body itself.
    ///
    /// The body handed to `f` is marked as nested: the frame's format is
    /// settled by the enclosing object, so nothing inside can switch it. A
    /// [`ResponseBody::write_typed_slice`] there writes a BEVE typed array
    /// inside a BEVE listing and a JSON array inside a JSON one.
    pub fn entry_with<F>(&mut self, key: &str, f: F) -> StructResult<()>
    where
        F: FnOnce(&mut ResponseBody<'_>) -> StructResult<()>,
    {
        self.key(key);
        let outcome = f(&mut self.value_body());
        if outcome.is_err() {
            self.buf.truncate(self.start);
        }
        outcome
    }

    /// [`entry_with`](Self::entry_with) for a value the source may decline to
    /// produce, as [`RepeStruct::repe_shared_into`] declines a path that needs
    /// exclusive access.
    ///
    /// `None` rewinds the whole object, exactly as an `Err` does, so a caller
    /// that declines in turn leaves the buffer as it found it without a second
    /// step. That is also why a declared count can never be short: an object
    /// that did not write every entry does not survive at all.
    pub fn entry_try_with<F>(&mut self, key: &str, f: F) -> Option<StructResult<()>>
    where
        F: FnOnce(&mut ResponseBody<'_>) -> Option<StructResult<()>>,
    {
        self.key(key);
        let outcome = f(&mut self.value_body());
        if !matches!(outcome, Some(Ok(()))) {
            self.buf.truncate(self.start);
        }
        outcome
    }

    /// Close the object.
    pub fn finish(self) {
        debug_assert_eq!(
            self.written, self.declared,
            "repe: a listing declared {} entries and wrote {}; under BEVE the \
             count is in the header and a disagreement corrupts the document",
            self.declared, self.written,
        );
        if self.format != BodyFormat::Beve {
            self.buf.push(b'}');
        }
    }
}

/// Compile-time check that the three sets of endpoints on one struct do not
/// overlap.
///
/// The two macros cannot see each other's input, so the derive emits a call to
/// this against everything *it* publishes — fields, plus any method named in a
/// struct-level `#[repe(methods(..))]` list — and the two generated
/// [`RepeMethods`] tables. Without it a collision is silent and costly: one
/// declaration wins dispatch, the other becomes permanently unreachable, and
/// the whole-struct listing emits the same key twice — a duplicate key in the
/// response body, which every listing now streams rather than deduplicating in
/// a map on the way out.
///
/// Collisions the macros *can* see on their own — two fields, two listed
/// methods, a field against a listed method, two endpoints in one
/// `#[repe::methods]` block — are rejected at macro time with a message naming
/// the endpoint and pointing at the second declaration.
///
/// The third pairing, methods against accessors, is unreachable from a
/// generated table for the same reason; it is checked because a hand-written
/// [`RepeMethods`] impl can produce it.
#[doc(hidden)]
pub const fn assert_no_endpoint_collision(
    declared: &[&str],
    methods: &[(&str, &str)],
    accessors: &[&str],
) {
    let mut i = 0;
    while i < methods.len() {
        if const_contains(declared, methods[i].0) {
            panic!(
                "a published method and an endpoint declared on the struct itself \
                 share a name: the struct's declaration wins dispatch and the method could \
                 never be called. Rename one with `#[repe(rename = \"...\")]`."
            );
        }
        i += 1;
    }

    let mut i = 0;
    while i < accessors.len() {
        if const_contains(declared, accessors[i]) {
            panic!(
                "a `#[repe(get = \"...\")]` accessor and an endpoint declared on the struct \
                 itself share a name: the struct's declaration wins dispatch and the accessor \
                 could never be called. Rename the struct's with \
                 `#[repe(rename = \"...\")]`, or give the accessor a different endpoint."
            );
        }

        if listed_signature(methods, accessors[i]).is_some() {
            panic!(
                "a published method and a `#[repe(get = \"...\")]` accessor in the \
                 same table share an endpoint name: one of them is unreachable, and the \
                 whole-struct listing would show the endpoint twice."
            );
        }
        i += 1;
    }
}

/// Compile-time check that a `#[repe(listing_order(..))]` list names exactly the
/// endpoints the struct publishes.
///
/// The derive validates the half it can see on its own — fields and struct-level
/// `#[repe(methods(..))]` entries — with an error that names the offending key.
/// It cannot see the `#[repe::methods]` impl block, so the other half is checked
/// here, where both generated tables are in scope. A `const` panic cannot name
/// the key, which is why the derive keeps everything it can reach at macro time.
///
/// Both directions matter. An order naming an endpoint that does not exist would
/// emit a key whose value came from a failed dispatch; an order omitting one
/// would silently drop it from every whole-object read, which is the harder bug
/// to see because the endpoint still answers on its own path.
#[doc(hidden)]
pub const fn assert_listing_order(
    order: &[&str],
    declared: &[&str],
    methods: &[(&str, &str)],
    accessors: &[&str],
    recovering: bool,
) {
    // `recovering` is [`RepeMethods::REPE_TABLE_RECOVERED`]: the block failed to
    // parse, so the tables describe nothing and every key here would look
    // unknown. This assertion would then be exactly the extra error that
    // recovery exists to prevent. It must come from the marker rather than from
    // the tables being empty — a block that compiles and publishes nothing is
    // also empty, and an unknown key in *that* struct's order has to be caught
    // here, because the derive skips its own check whenever an impl block is in
    // play.
    let mut i = 0;
    while i < order.len() {
        let known = const_contains(declared, order[i])
            || const_contains(accessors, order[i])
            || listed_signature(methods, order[i]).is_some();
        if !known && !recovering {
            panic!(
                "`#[repe(listing_order(..))]` names a key that is not an endpoint on this \
                 struct. It lists the whole-object listing's keys, so every name in it has to \
                 be a field, a listed method, or an endpoint of the annotated `methods` impl block."
            );
        }
        i += 1;
    }

    let mut j = 0;
    while j < methods.len() {
        if !const_contains(order, methods[j].0) {
            panic!(
                "a method on the annotated `methods` impl block is missing from `#[repe(listing_order(..))]`, \
                 which names the whole-object listing's keys in full. An omitted endpoint \
                 would disappear from every whole-object read while still answering on its \
                 own path."
            );
        }
        j += 1;
    }

    let mut j = 0;
    while j < accessors.len() {
        if !const_contains(order, accessors[j]) {
            panic!(
                "a `#[repe(get = \"...\")]` accessor is missing from \
                 `#[repe(listing_order(..))]`, which names the whole-object listing's keys in \
                 full. An omitted endpoint would disappear from every whole-object read while \
                 still answering on its own path."
            );
        }
        j += 1;
    }
}

/// The published signature for `name`, or `None` when the name belongs to a
/// field-shaped accessor instead.
///
/// Called from a `const` block in the ordered listing a
/// `#[repe(listing_order(..))]` produces: the derive cannot see the
/// `#[repe::methods]` block, so it does not know which of the two tables an
/// ordered key came from, but the tables themselves are constants and the
/// question folds away before run time.
#[doc(hidden)]
pub const fn listed_signature<'a>(methods: &[(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < methods.len() {
        if const_str_eq(methods[i].0, name) {
            return Some(methods[i].1);
        }
        i += 1;
    }
    None
}

/// `slice.contains(&name)` usable in a `const` context.
const fn const_contains(list: &[&str], name: &str) -> bool {
    let mut i = 0;
    while i < list.len() {
        if const_str_eq(list[i], name) {
            return true;
        }
        i += 1;
    }
    false
}

/// `str` equality usable in a `const` context, which `PartialEq` is not.
const fn const_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Decoder for the body of a struct method taking two or more arguments.
///
/// A single-argument method takes the request body *as* the argument, which is
/// the shape the wire has always had. Beyond one argument the body carries the
/// whole list, in either of two forms:
///
/// * a JSON/BEVE **array** of exactly N values, positionally, or
/// * an **object** keyed by parameter name, where a missing key decodes as
///   `null` (so an `Option<T>` parameter may be omitted) and a repeated key
///   takes its last value.
///
/// All three BEVE array encodings work: generic, typed, and complex. They are
/// not read the same way — a generic array's elements are self-contained values
/// in the input and are sliced out as spans, while a typed or complex array's
/// element headers are supplied by the reader rather than present in the bytes,
/// so those are pulled through `structio::beve::Documents::array` instead — but
/// that is an implementation detail and not a wire distinction. A client encodes
/// a homogeneous argument list however its library naturally would.
///
/// Generated code names this, and a hand-written [`RepeMethods`] impl that
/// publishes a multi-argument method should too: it is what makes such a table
/// accept the same two body shapes a derived one does, instead of inventing a
/// third that no client of a derived struct would speak. That is why it is
/// public and documented rather than hidden — it carries a wire contract, not an
/// implementation detail.
///
/// # How it works without a tree
///
/// It used to split a `serde_json::Value` into owned `Value`s. There is no tree
/// now, so it does the same job by **recording byte spans**: one pass over the
/// body notes where each element starts and ends, and
/// [`next_arg`](Self::next_arg) parses one span into one argument. Nothing
/// between the two is materialized, no key is copied, and an argument the caller
/// never asks for is never parsed.
///
/// The spans borrow the frame, which is why this carries the body's lifetime.
pub struct MethodArgs<'a> {
    path: &'a str,
    names: &'a [&'a str],
    format: BodyFormat,
    source: ArgSource<'a>,
    index: usize,
}

/// Where [`MethodArgs::next_arg`] gets one argument's bytes.
///
/// Two arms, because BEVE has two kinds of array. In a generic array — and in
/// every JSON body — an element is a self-contained value in the input, so a
/// span cut between two reader positions is a document and can be parsed on its
/// own. In a *typed* or *complex* array it is not: the element's header is
/// supplied by the reader rather than present in the bytes, so those spans are
/// headerless payload, and a boolean run's are empty and not even monotonic.
///
/// `structio::beve::Documents::array` is the seam for the second kind. It hands
/// each element out as a document of its own with the header installed, and
/// `next_value_into` is generic per call, so a heterogeneous argument list still
/// works. It costs a copy of the body into a window sized to the body — which is
/// an argument list, never a bulk payload, since a method taking one large array
/// takes the body *as* its single argument and never reaches here.
enum ArgSource<'a> {
    /// One slot per name, in declaration order. `None` is a key the object form
    /// left out, which [`MethodArgs::next_arg`] reads as `null`.
    Spans(Vec<Option<&'a [u8]>>),
    /// Elements pulled in order from an array whose headers the reader supplies.
    Cursor(structio::beve::Documents<&'a [u8]>),
}

impl<'a> MethodArgs<'a> {
    /// Split `body` into the argument list for `names`.
    pub fn new(path: &'a str, names: &'a [&'a str], body: RequestBody<'a>) -> StructResult<Self> {
        let source = match body.format() {
            BodyFormat::Beve => split_beve(path, names, body.bytes())?,
            _ => ArgSource::Spans(split_json(path, names, body.bytes())?),
        };
        Ok(Self {
            path,
            names,
            format: body.format(),
            source,
            index: 0,
        })
    }

    /// Decode the next argument in declaration order.
    pub fn next_arg<T>(&mut self) -> StructResult<T>
    where
        T: for<'de> ServableRead<'de> + Default,
    {
        let name = self.names.get(self.index).copied().unwrap_or("?");
        let index = self.index;
        self.index += 1;
        let mut value = T::default();
        // The label is built on the error branch only: a multi-argument call
        // would otherwise format one `String` per argument on every success.
        let label = || format!("{}({name})", self.path);

        match &mut self.source {
            ArgSource::Spans(spans) => {
                // An omitted key is the JSON `null` whatever the frame's format
                // is: the stand-in never came off the wire, so there is no BEVE
                // encoding of it to prefer.
                let (bytes, format) = match spans.get(index).copied().flatten() {
                    Some(bytes) => (bytes, self.format),
                    None => (&b"null"[..], BodyFormat::Json),
                };
                RequestBody::new(bytes, format)
                    .read_into_raw(&mut value)
                    .map_err(|source| StructError::Decode {
                        path: label(),
                        source,
                    })?;
            }
            ArgSource::Cursor(documents) => {
                // Arity was settled in `split_beve`, so running out here would
                // mean the count in the header disagreed with the elements
                // behind it — a malformed document, not a short call.
                let outcome =
                    documents
                        .next_value_into(&mut value)
                        .ok_or_else(|| StructError::Decode {
                            path: label(),
                            source: structio::Error::new(structio::ErrorCode::UnexpectedEnd, 0),
                        })?;
                outcome.map_err(|err| StructError::Decode {
                    path: label(),
                    source: stream_parse_error(err),
                })?;
            }
        }
        Ok(value)
    }
}

/// Turn a structio parse failure into the error this module reports. Callers
/// pass a whole-body offset, which is what the parser's `position` already is.
fn decode_error(path: &str, code: structio::ErrorCode, at: usize) -> StructError {
    StructError::Decode {
        path: path.to_string(),
        source: structio::Error::new(code, at),
    }
}

/// Record one span per name from a JSON argument body.
fn split_json<'a>(
    path: &str,
    names: &[&str],
    bytes: &'a [u8],
) -> StructResult<Vec<Option<&'a [u8]>>> {
    let text = core::str::from_utf8(bytes)
        .map_err(|err| decode_error(path, structio::ErrorCode::InvalidUtf8, err.valid_up_to()))?;
    let mut p = structio::json::Parser::new(text);
    p.skip_ws();

    // Decide the shape from the opening byte rather than from whether a walk
    // succeeded. A walk can fail half way through a body whose shape was never
    // in doubt — one malformed element in an otherwise well-formed array — and
    // reporting that as "neither an array nor an object" hides the real fault.
    match p.rest().first() {
        Some(b'[') => {
            let mut positional: Vec<Option<&'a [u8]>> = Vec::new();
            p.read_seq(|p, _| {
                let start = p.position();
                p.skip_value()?;
                positional.push(Some(&bytes[start..p.position()]));
                Ok(())
            })
            .and_then(|_| p.finish())
            .map_err(|code| decode_error(path, code, p.position()))?;
            check_arity(path, names, positional.len())?;
            Ok(positional)
        }
        Some(b'{') => {
            let mut spans: Vec<Option<&'a [u8]>> = vec![None; names.len()];
            p.read_map(|p, key| {
                // Matched during the walk, so a key no parameter has costs
                // nothing and a repeated key overwrites — last-wins, as the
                // `serde_json::Map` this replaced did.
                let slot = names.iter().position(|name| *name == key.as_str());
                let start = p.position();
                p.skip_value()?;
                if let Some(i) = slot {
                    spans[i] = Some(&bytes[start..p.position()]);
                }
                Ok(())
            })
            .and_then(|()| p.finish())
            .map_err(|code| decode_error(path, code, p.position()))?;
            Ok(spans)
        }
        _ => Err(shape_error(path, names)),
    }
}

/// A parse failure out of a streaming read, as this module reports it.
///
/// The source is an in-memory slice, so `StreamError::Io` is unreachable in
/// practice; it is mapped rather than unwrapped because a reader is entitled to
/// report one and a panic here would be a worse answer than a decode error.
fn stream_parse_error(err: structio::StreamError) -> structio::Error {
    match err {
        structio::StreamError::Parse(err) => err,
        _ => structio::Error::new(structio::ErrorCode::UnexpectedEnd, 0),
    }
}

/// Decide how a BEVE argument body is split.
fn split_beve<'a>(path: &str, names: &[&str], bytes: &'a [u8]) -> StructResult<ArgSource<'a>> {
    // The shape is decided from the header, for the reason `split_json` decides
    // it from the opening byte, and for a second reason particular to BEVE.
    //
    // `Reader::read_seq` accepts a *typed* array as readily as a generic one,
    // and a *complex* array through the same door, installing each element's
    // header out of band rather than reading it from the input. A span cut
    // between two reader positions is therefore not a document those elements
    // can be read from — for a typed array of booleans the spans are empty and
    // not even monotonic. Those two shapes go to `Documents::array`, which hands
    // each element out with the header installed; only the shapes whose elements
    // are self-contained in the input are sliced.
    match bytes.first().map(|&h| structio::beve::header::ty(h)) {
        Some(structio::beve::header::TY_GENERIC_ARRAY) => {
            let mut positional: Vec<Option<&'a [u8]>> = Vec::new();
            let mut r = structio::beve::Reader::new(bytes);
            r.read_seq(|r, _| {
                let start = r.position();
                r.skip_value()?;
                positional.push(Some(&bytes[start..r.position()]));
                Ok(())
            })
            .and_then(|_| r.finish())
            .map_err(|code| decode_error(path, code, r.position()))?;
            check_arity(path, names, positional.len())?;
            Ok(ArgSource::Spans(positional))
        }
        Some(structio::beve::header::TY_OBJECT) => {
            let mut spans: Vec<Option<&'a [u8]>> = vec![None; names.len()];
            let mut r = structio::beve::Reader::new(bytes);
            r.read_map(|r, key| {
                // Argument names are identifiers, so only the string form of a
                // BEVE key can match one; a numeric key matches nothing and its
                // value is skipped like any other unclaimed key.
                let slot = match key {
                    structio::beve::Key::Str(key) => names.iter().position(|name| *name == key),
                    _ => None,
                };
                let start = r.position();
                r.skip_value()?;
                if let Some(i) = slot {
                    spans[i] = Some(&bytes[start..r.position()]);
                }
                Ok(())
            })
            .and_then(|()| r.finish())
            .map_err(|code| decode_error(path, code, r.position()))?;
            Ok(ArgSource::Spans(spans))
        }
        // The two header-synthesizing forms. `TY_EXTENSION` is where
        // `header::COMPLEX` lives.
        Some(structio::beve::header::TY_TYPED_ARRAY | structio::beve::header::TY_EXTENSION) => {
            // Arity is settled before any argument is decoded, as it is for
            // every other shape, so a wrong-length call fails the same way
            // whichever encoding it arrived in. This walk skips elements rather
            // than reading them; it is one pass over an argument list.
            let mut r = structio::beve::Reader::new(bytes);
            let count = r
                .read_seq(|r, _| r.skip_value())
                .and_then(|count| r.finish().map(|()| count))
                .map_err(|code| decode_error(path, code, r.position()))?;
            check_arity(path, names, count)?;
            // Sized to the body, so the first fill takes it whole and the window
            // never grows. The body is an argument list, so this is bounded by
            // the arity and the element width, not by any payload.
            Ok(ArgSource::Cursor(
                structio::beve::Documents::array(bytes).read_size(bytes.len().max(1)),
            ))
        }
        _ => Err(shape_error(path, names)),
    }
}

fn check_arity(path: &str, names: &[&str], got: usize) -> StructResult<()> {
    if got == names.len() {
        Ok(())
    } else {
        Err(StructError::Arguments {
            path: path.to_string(),
            message: format!("expected {} arguments, got {got}", names.len()),
        })
    }
}

fn shape_error(path: &str, names: &[&str]) -> StructError {
    StructError::Arguments {
        path: path.to_string(),
        message: format!(
            "expected an array of {} arguments or an object with keys [{}]",
            names.len(),
            names.join(", ")
        ),
    }
}
