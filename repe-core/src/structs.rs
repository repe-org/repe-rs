use crate::constants::{BodyFormat, ErrorCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
/// Errors produced while handling struct-backed endpoints.
///
/// Non-exhaustive: match with a wildcard arm, or use [`StructError::code`] to
/// map any error onto its protocol [`ErrorCode`].
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
    #[error("serialization error for `{path}`: {source}")]
    Serialize {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("deserialization error for `{path}`: {source}")]
    Deserialize {
        path: String,
        #[source]
        source: serde_json::Error,
    },
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
            StructError::Serialize { .. } | StructError::Deserialize { .. } => {
                ErrorCode::InvalidBody
            }
            StructError::Execution { .. } => ErrorCode::ParseError,
        }
    }
}

/// Convenience alias for results returned by struct handlers.
pub type StructResult<T> = Result<T, StructError>;

/// Trait implemented by structs that can be exposed directly through the REPE router.
///
/// The implementation should interpret the provided JSON Pointer path segments and
/// either return a JSON value (for reads) or mutate the struct (for writes).
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be served over REPE: it does not implement `RepeStruct`",
    label = "not a REPE-servable type",
    note = "derive it with `#[derive(RepeStruct)]`, or, for a field of a type you do not own, use `#[repe(nested_serde)]` to route through its `Serialize`/`Deserialize` impls instead of `#[repe(nested)]`",
    note = "a type in a crate that depends only on `repe-core` can derive this too; `repe` re-exports the same trait"
)]
pub trait RepeStruct: Send + Sync {
    /// Handle a read or write against this struct.
    ///
    /// * `segments` – JSON Pointer path split into unescaped segments.
    /// * `body` – `Some(value)` when the request contains a JSON body, `None` for reads.
    ///
    /// Return `Ok(Some(value))` to send a JSON response payload, `Ok(None)` to send `null`.
    fn repe_handle(
        &mut self,
        segments: &[&str],
        body: Option<Value>,
    ) -> StructResult<Option<Value>>;

    /// Encode-in-place counterpart of [`repe_handle`](Self::repe_handle): write the
    /// response body straight into `out` instead of returning an owned
    /// [`serde_json::Value`] for the caller to re-serialize.
    ///
    /// This is the method the router calls. The default implementation delegates
    /// to [`repe_handle`](Self::repe_handle) and encodes whatever it returns, so a
    /// hand-written impl only needs `repe_handle`; `#[derive(RepeStruct)]`
    /// overrides it so a leaf read serializes the live field directly into the
    /// outgoing frame buffer with no intermediate `Value`.
    ///
    /// Writes are unaffected — a request body still arrives as a `Value`.
    ///
    /// [`repe_handle`]: Self::repe_handle
    fn repe_handle_into(
        &mut self,
        segments: &[&str],
        body: Option<Value>,
        out: &mut ResponseBody<'_>,
    ) -> StructResult<()> {
        let value = self.repe_handle(segments, body)?;
        write_optional(out, segments, value)
    }

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
    /// alone put a long-running `&self` call behind the write guard and
    /// stalled every read of the object for as long as it ran. The receiver is known
    /// where these arms are generated, so it is the receiver that decides.
    ///
    /// Return `None` for any path this borrow cannot serve: a field write, a
    /// `&mut self` method, a nested field whose child declined. The router then
    /// retakes the lock exclusively and dispatches through
    /// [`repe_handle_into`](Self::repe_handle_into), so declining costs
    /// correctness nothing — it is the default, and a hand-written impl that
    /// never overrides this behaves exactly as it did before the path existed.
    ///
    /// Three obligations come with overriding it:
    ///
    /// * **Answer identically.** Whatever this returns for a path is what the
    ///   client sees; the exclusive path is not consulted afterwards. Decline
    ///   rather than approximate.
    /// * **Write nothing when declining.** A `None` return must leave `out` as
    ///   it was found, so the exclusive attempt starts from an empty body.
    ///   [`ObjectBody::entry_try_with`] exists to make that mechanical when the
    ///   decline surfaces partway through an object: it rewinds the whole
    ///   object, so propagating its `None` is all a caller has to do.
    /// * **Leave the body alone when declining.** `body` is handed over as
    ///   `&mut Option<Value>` rather than by value precisely so the exclusive
    ///   retry can re-dispatch the same request without a clone. Call
    ///   [`Option::take`] on it only once this borrow has committed to
    ///   answering — an `Err` counts as an answer — and never on a path that
    ///   goes on to return `None`. A body taken and then declined reaches the
    ///   exclusive path as `None`, where it surfaces as a spurious
    ///   [`BodyExpected`](StructError::BodyExpected).
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
        body: &mut Option<Value>,
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
    /// on: returning `false` asserts that `repe_shared_into(&[], &mut None, ..)`
    /// answers `Some(..)`. Breaking it still yields a correct response — the listing
    /// rewinds and the exclusive path retries — but anything the shared attempt
    /// had already invoked runs twice.
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
    fn repe_handle(
        &mut self,
        segments: &[&str],
        body: Option<Value>,
    ) -> StructResult<Option<Value>> {
        refuse_presence_write(segments, body.as_ref())?;
        match self {
            Some(inner) => inner.repe_handle(segments, body),
            None => absent_child(segments, body.is_some()).map(|()| None),
        }
    }

    fn repe_handle_into(
        &mut self,
        segments: &[&str],
        body: Option<Value>,
        out: &mut ResponseBody<'_>,
    ) -> StructResult<()> {
        refuse_presence_write(segments, body.as_ref())?;
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
        body: &mut Option<Value>,
        out: &mut ResponseBody<'_>,
    ) -> Option<StructResult<()>> {
        // Refusing touches nothing, so it needs no exclusive borrow — and it has
        // to be decided here rather than left to the retry, or a present child
        // would forward the `null` to its inner value under both guards.
        if let Err(err) = refuse_presence_write(segments, body.as_ref()) {
            return Some(Err(err));
        }
        match self {
            Some(inner) => inner.repe_shared_into(segments, body, out),
            // Absent is a constant: no state to borrow exclusively, so every
            // answer above is servable here and none of them touches `body`.
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
/// type's serde complaint, an absent one answers `InvalidPath`. Neither says
/// what is actually true, which is that presence is not settable here.
///
/// It is refused rather than honoured because honouring it would open a one-way
/// door: `null` could remove a child, and nothing could put it back — creating
/// one is the write an absent child already refuses, for the reason that a
/// client configuring something that is not there must not be told it worked.
/// Presence belongs to the host on both sides of that. A whole-object write at
/// the *parent* still replaces the field, `null` included; that is a replace of
/// the parent, not a write to this path.
fn refuse_presence_write(segments: &[&str], body: Option<&Value>) -> StructResult<()> {
    if segments.is_empty() && matches!(body, Some(Value::Null)) {
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
    /// those values back through [`repe_call`](Self::repe_call), or through
    /// [`repe_call_shared_into`](Self::repe_call_shared_into) when it is being
    /// served under a shared borrow.
    ///
    /// Defaults to empty, so a hand-written impl need not mention it. One
    /// invariant comes with listing a name here, and the derived listing relies
    /// on it: [`repe_call`](Self::repe_call) must answer
    /// `segments = &[name], body = None` with `Ok(Some(value))`. A name the
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
    /// exclusive path, which is what a hand-written table gets for free and what
    /// this crate did for every table before the receiver was carried here.
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

    /// Invoke the method named by `segments[0]`.
    ///
    /// `segments` is the remaining path relative to the struct root, so error
    /// paths match the ones [`RepeStruct::repe_handle`] produces for fields.
    /// Returns [`StructError::InvalidPath`] when no method matches.
    fn repe_call(&mut self, segments: &[&str], body: Option<Value>) -> StructResult<Option<Value>>;

    /// Encode-in-place counterpart of [`repe_call`](Self::repe_call), mirroring
    /// [`RepeStruct::repe_handle_into`].
    fn repe_call_into(
        &mut self,
        segments: &[&str],
        body: Option<Value>,
        out: &mut ResponseBody<'_>,
    ) -> StructResult<()> {
        let value = self.repe_call(segments, body)?;
        write_optional(out, segments, value)
    }

    /// Shared-borrow counterpart of [`repe_call_into`](Self::repe_call_into),
    /// mirroring [`RepeStruct::repe_shared_into`] and carrying the same three
    /// obligations: answer identically, decline without writing, and leave
    /// `body` in place when declining.
    ///
    /// `#[repe::methods]` serves every `&self` method here — including one
    /// taking arguments, which is a call rather than a mutation however the
    /// frame is shaped — along with the getter half of a `&self` accessor, and
    /// declines everything else.
    fn repe_call_shared_into(
        &self,
        segments: &[&str],
        body: &mut Option<Value>,
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
    if segments.is_empty() {
        String::from("")
    } else {
        let mut s = String::new();
        for segment in segments {
            s.push('/');
            s.push_str(segment);
        }
        s
    }
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
        | StructError::Serialize { path, .. }
        | StructError::Deserialize { path, .. }
        | StructError::Execution { path, .. } => {
            *path = with_prefix(std::mem::take(path));
        }
    }
    err
}

/// The bound on [`ResponseBody::write_typed_slice`] without the `typed`
/// feature. Deliberately unimplementable — it is sealed, so no type satisfies
/// it *and none can be made to* — which makes the method uncallable and a
/// `#[repe(typed)]` field an error against a named feature rather than against
/// a method that does not exist.
///
/// It exists **only** in that configuration. With the feature on, the method
/// takes `beve::BeveTypedSlice + Serialize` directly and this trait is not
/// there at all — a diagnostic device has no business being public API of the
/// build everyone ships, and, since every type BEVE accepts would satisfy it
/// through a blanket impl, nothing would ever name it there anyway.
/// Seals [`TypedSliceElement`] so that "no type implements it" is a fact this
/// crate enforces rather than one it merely asserts. Without this a downstream
/// crate could write `impl TypedSliceElement for T {}` — the trait is public and
/// has no members, so nothing stops it — and reach a `write_typed_slice` whose
/// body is an `unreachable!`.
#[cfg(not(feature = "typed"))]
mod sealed {
    pub trait Sealed {}
}

#[cfg(not(feature = "typed"))]
#[diagnostic::on_unimplemented(
    message = "`#[repe(typed)]` needs the `typed` feature of `repe-core`",
    label = "no BEVE typed-array encoding is compiled in",
    note = "enable it with `repe-core = {{ version = \"2\", features = [\"typed\"] }}`, or drop \
            `#[repe(typed)]` from the field to serve it as a JSON array",
    note = "crates that depend on `repe` rather than `repe-core` already have it on"
)]
pub trait TypedSliceElement: sealed::Sealed {}

/// Response body under construction for a [`RepeStruct`] read.
///
/// Handed to [`RepeStruct::repe_handle_into`] so a read serializes the live field
/// straight into the outgoing frame buffer. The alternative — returning a
/// [`serde_json::Value`] that the router then re-serializes — allocates an
/// intermediate tree per read, which is what a client polling a wide object at
/// kilohertz pays for.
///
/// The body starts out empty and [`BodyFormat::Json`]; the write that fills it
/// also settles the format, so a [`write_typed_slice`](Self::write_typed_slice)
/// body reports [`BodyFormat::Beve`]. Exactly one top-level write is expected —
/// a value, a null, or an [`object`](Self::object).
///
/// Any failed write — including one that fails partway through an
/// [`object`](Self::object) — rewinds the buffer to where that write began, so a
/// caller that recovers from a [`StructError`] never ships half-serialized
/// bytes.
pub struct ResponseBody<'a> {
    buf: &'a mut Vec<u8>,
    format: BodyFormat,
    /// True while this body is a value *inside* an enclosing JSON object, where
    /// the frame format is already committed to JSON.
    ///
    /// Reached from generated code by exactly one path: the whole-struct listing
    /// writes each accessor endpoint's value through
    /// [`ObjectBody::entry_with`] or [`ObjectBody::entry_try_with`], so a
    /// `#[repe(typed)]` getter listed inside the object emits a JSON array
    /// rather than switching the enclosing frame to BEVE. (A `#[repe(nested)]`
    /// field also gets a nested body, but its child is always entered with empty
    /// segments and so always takes its own object branch.) The flag also covers
    /// hand-written `repe_handle_into` impls, which can call anything from
    /// anywhere; a JSON-array fallback beats corrupting the frame.
    ///
    /// [`write_typed_slice`](Self::write_typed_slice) is the only reader, and
    /// without the `typed` feature there is no typed encoding to divert — the
    /// flag is still set where an object is entered, so that turning the
    /// feature on needs no other change.
    ///
    /// `expect` rather than `allow`: if this ever gains a second reader that is
    /// not feature-gated, the attribute becomes wrong and says so.
    #[cfg_attr(not(feature = "typed"), expect(dead_code))]
    nested: bool,
}

impl<'a> ResponseBody<'a> {
    /// Wrap an output buffer. `buf` is normally empty; writes append to it.
    pub fn new(buf: &'a mut Vec<u8>) -> Self {
        Self {
            buf,
            format: BodyFormat::Json,
            nested: false,
        }
    }

    /// The body format settled by the write that filled this body.
    pub fn format(&self) -> BodyFormat {
        self.format
    }

    /// Serialize `value` as the whole body, as JSON.
    ///
    /// `path` names the field for the error message and is only read when
    /// serialization fails.
    pub fn write<T>(&mut self, path: &str, value: &T) -> StructResult<()>
    where
        T: Serialize + ?Sized,
    {
        self.write_with_path(|| String::from(path), value)
    }

    /// [`write`](Self::write) with the error path built only if one is needed.
    ///
    /// The derived arms pass a `&'static str` literal and pay nothing either
    /// way, but the [`RepeStruct::repe_handle_into`] default has to *assemble*
    /// its path from the segments — on every successful read, for a string it
    /// then throws away.
    fn write_with_path<T, F>(&mut self, path: F, value: &T) -> StructResult<()>
    where
        T: Serialize + ?Sized,
        F: FnOnce() -> String,
    {
        let start = self.buf.len();
        match serde_json::to_writer(&mut *self.buf, value) {
            Ok(()) => Ok(()),
            Err(source) => {
                self.buf.truncate(start);
                Err(StructError::Serialize {
                    path: path(),
                    source,
                })
            }
        }
    }

    /// Write a JSON `null` body — the response to a write, or to a read that has
    /// nothing to return.
    pub fn write_null(&mut self) {
        self.buf.extend_from_slice(b"null");
    }

    /// Write `slice` as a BEVE typed numeric array and set the body format to
    /// [`BodyFormat::Beve`].
    ///
    /// The same bulk encode `repe::message::MessageBuilder::body_typed_slice`
    /// produces — one `copy_nonoverlapping` on little-endian targets rather than
    /// a per-element serde walk, and byte-identical to what Glaze emits for the
    /// same array. This is what `#[repe(typed)]` routes a numeric field to.
    /// (Named rather than linked: the builder lives in `repe`, which this crate
    /// deliberately does not depend on.)
    ///
    /// Inside an enclosing object — the whole-struct listing — the frame is
    /// already committed to JSON, so the slice is written as a JSON array
    /// instead. A field only reaches the typed encoding when it is read on its
    /// own.
    ///
    /// Requires the `typed` feature, which is what carries `beve`. It is on
    /// through `repe` and off for a direct `repe-core` dependency, where this
    /// signature is replaced by one whose bound no type satisfies, so that
    /// calling it is a compile error naming the feature.
    #[cfg(feature = "typed")]
    pub fn write_typed_slice<T>(&mut self, path: &str, slice: &[T]) -> StructResult<()>
    where
        T: beve::BeveTypedSlice + Serialize,
    {
        if self.nested {
            return self.write(path, slice);
        }
        let encoded_len = beve::typed_slice_size(slice) as usize;
        self.buf.reserve(encoded_len);
        let start = self.buf.len();
        beve::to_writer_typed_slice(&mut *self.buf, slice)
            .expect("writing a typed slice into a Vec is infallible");
        debug_assert_eq!(self.buf.len() - start, encoded_len);
        self.format = BodyFormat::Beve;
        Ok(())
    }

    /// The `typed`-less stand-in for [`write_typed_slice`], kept so a
    /// `#[repe(typed)]` field reports a missing *feature* rather than a missing
    /// *method*.
    ///
    /// No type implements [`TypedSliceElement`] without the feature, so this is
    /// uncallable and the diagnostic on that trait is what a caller actually
    /// reads.
    ///
    /// **The `E0277` this produces is the deliverable, not a side effect.**
    /// `#[cfg]`-ing this method away instead is the obvious simplification and
    /// silently degrades it to `E0599`, "no method named `write_typed_slice`",
    /// pointing at a derive rather than at a feature — and nothing in CI would
    /// notice. Falling back to the JSON array here would be worse still: it
    /// compiles and changes the wire.
    ///
    /// [`write_typed_slice`]: ResponseBody::write_typed_slice
    #[cfg(not(feature = "typed"))]
    pub fn write_typed_slice<T>(&mut self, path: &str, slice: &[T]) -> StructResult<()>
    where
        T: TypedSliceElement,
    {
        let _ = (path, slice);
        unreachable!("`TypedSliceElement` is sealed and implemented by nothing")
    }

    /// Begin a JSON object body, written key by key.
    ///
    /// Used by the whole-struct listing so a wide object is emitted in one pass
    /// instead of through a [`serde_json::Map`].
    pub fn object(&mut self) -> ObjectBody<'_> {
        let start = self.buf.len();
        self.buf.push(b'{');
        ObjectBody {
            buf: self.buf,
            start,
            first: true,
        }
    }
}

/// A JSON object body being written key by key. Created by
/// [`ResponseBody::object`]; call [`finish`](Self::finish) to close it.
///
/// An entry that fails — or that [`entry_try_with`](Self::entry_try_with)
/// declines — rewinds the buffer past the opening brace, so a partly written
/// object is never left behind for a caller that recovers from it. Dropping
/// without calling [`finish`](Self::finish) is the same situation and leaves
/// nothing behind either, since both ways out without finishing have already
/// rewound.
pub struct ObjectBody<'a> {
    buf: &'a mut Vec<u8>,
    /// Buffer length before the opening brace, to rewind to on failure.
    start: usize,
    first: bool,
}

impl ObjectBody<'_> {
    fn key(&mut self, key: &str) -> StructResult<()> {
        if self.first {
            self.first = false;
        } else {
            self.buf.push(b',');
        }
        serde_json::to_writer(&mut *self.buf, key).map_err(|source| StructError::Serialize {
            path: format!("/{key}"),
            source,
        })?;
        self.buf.push(b':');
        Ok(())
    }

    /// Run one entry, rewinding the whole object if it fails.
    fn entry_with_rewind<F>(&mut self, f: F) -> StructResult<()>
    where
        F: FnOnce(&mut Self) -> StructResult<()>,
    {
        match f(self) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.buf.truncate(self.start);
                Err(err)
            }
        }
    }

    /// Write one `"key": value` entry.
    pub fn entry<T>(&mut self, key: &str, value: &T) -> StructResult<()>
    where
        T: Serialize + ?Sized,
    {
        self.entry_with_rewind(|this| {
            this.key(key)?;
            serde_json::to_writer(&mut *this.buf, value).map_err(|source| StructError::Serialize {
                path: format!("/{key}"),
                source,
            })
        })
    }

    /// Write one `"key": <value>` entry whose value is produced by `f`, for a
    /// nested [`RepeStruct`] that fills the body itself.
    ///
    /// The body handed to `f` is marked as nested: the enclosing frame is
    /// already JSON, so a [`ResponseBody::write_typed_slice`] inside it falls
    /// back to a JSON array rather than switching the frame to BEVE.
    pub fn entry_with<F>(&mut self, key: &str, f: F) -> StructResult<()>
    where
        F: FnOnce(&mut ResponseBody<'_>) -> StructResult<()>,
    {
        self.entry_with_rewind(|this| {
            this.key(key)?;
            let mut body = ResponseBody {
                buf: this.buf,
                format: BodyFormat::Json,
                nested: true,
            };
            f(&mut body)
        })
    }

    /// [`entry_with`](Self::entry_with) for a value the source may decline to
    /// produce, as [`RepeStruct::repe_shared_into`] declines a path that needs
    /// exclusive access.
    ///
    /// `None` rewinds the whole object, exactly as an `Err` does, so a caller
    /// that declines in turn leaves the buffer as it found it without a second
    /// step. Dropping the object without calling [`finish`](Self::finish) then
    /// leaves nothing behind, because the only ways out without finishing are
    /// an error and a decline, and both have already rewound.
    pub fn entry_try_with<F>(&mut self, key: &str, f: F) -> Option<StructResult<()>>
    where
        F: FnOnce(&mut ResponseBody<'_>) -> Option<StructResult<()>>,
    {
        let outcome = match self.key(key) {
            Ok(()) => {
                let mut body = ResponseBody {
                    buf: self.buf,
                    format: BodyFormat::Json,
                    nested: true,
                };
                f(&mut body)
            }
            Err(err) => Some(Err(err)),
        };
        if !matches!(outcome, Some(Ok(()))) {
            self.buf.truncate(self.start);
        }
        outcome
    }

    /// Close the object.
    pub fn finish(self) {
        self.buf.push(b'}');
    }
}

/// Write the `Option<Value>` a [`RepeStruct::repe_handle`] returned as the whole
/// body: the payload, or `null` when there is nothing to send.
///
/// Shared by the [`RepeStruct::repe_handle_into`] and
/// [`RepeMethods::repe_call_into`] defaults, which differ only in which method
/// they delegate to.
fn write_optional(
    out: &mut ResponseBody<'_>,
    segments: &[&str],
    value: Option<Value>,
) -> StructResult<()> {
    match value {
        Some(value) => out.write_with_path(|| path_from_segments(segments), &value),
        None => {
            out.write_null();
            Ok(())
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
/// the whole-struct listing emits the same key twice. The two listings do not
/// even agree on that last part, since a `serde_json::Map` deduplicates the key
/// and the streaming encoder does not.
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

/// Resolve path `segments` against a [`serde_json::Value`], for a
/// `#[repe(nested_serde)]` field.
///
/// Objects are indexed by key and arrays by decimal position, matching RFC 6901
/// evaluation; this takes already-split segments because the struct router has
/// them in that form and the pointer string does not exist.
///
/// `None` means the path does not resolve, which the caller reports as
/// [`StructError::InvalidPath`].
pub fn serde_pointer<'a>(value: &'a Value, segments: &[&str]) -> Option<&'a Value> {
    let mut cursor = value;
    for segment in segments {
        cursor = match cursor {
            Value::Object(map) => map.get(*segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cursor)
}

/// The mutable counterpart of [`serde_pointer`], for a write below a
/// `#[repe(nested_serde)]` field.
///
/// `segments` must be non-empty: replacing the whole value is the caller's own
/// assignment, not a walk. Every segment, the last included, has to name
/// something that is **already there** — a key an object does not have is
/// rejected rather than inserted, because the value is about to be deserialized
/// back into a typed field where an invented key is either dropped or an error,
/// and a client that misspelled one deserves to hear about it here.
///
/// Contrast [`crate::structs`]' sibling in `repe`'s registry, `set_pointer`,
/// which deliberately *inserts* a missing object key: that one is building a
/// free-form document, this one is editing a value with a type. Do not unify
/// them.
pub fn serde_pointer_set(value: &mut Value, segments: &[&str], new: Value) -> Option<()> {
    if segments.is_empty() {
        return None;
    }
    let mut cursor = value;
    for segment in segments {
        cursor = match cursor {
            Value::Object(map) => map.get_mut(*segment)?,
            Value::Array(items) => items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    *cursor = new;
    Some(())
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
///   `null` (so an `Option<T>` parameter may be omitted).
///
/// Generated code names this, and a hand-written [`RepeMethods`] impl that
/// publishes a multi-argument method should too: it is what makes such a table
/// accept the same two body shapes a derived one does, instead of inventing a
/// third that no client of a derived struct would speak. That is why it is
/// public and documented rather than hidden — it carries a wire contract, not
/// an implementation detail.
pub struct MethodArgs<'a> {
    path: &'a str,
    names: &'a [&'a str],
    values: std::vec::IntoIter<Value>,
    index: usize,
}

impl<'a> MethodArgs<'a> {
    /// Split `body` into the argument list for `names`.
    pub fn new(path: &'a str, names: &'a [&'a str], body: Value) -> StructResult<Self> {
        let values = match body {
            Value::Array(items) => {
                if items.len() != names.len() {
                    return Err(arg_error(
                        path,
                        format!("expected {} arguments, got {}", names.len(), items.len()),
                    ));
                }
                items
            }
            Value::Object(mut map) => names
                .iter()
                .map(|name| map.remove(*name).unwrap_or(Value::Null))
                .collect(),
            _ => {
                return Err(arg_error(
                    path,
                    format!(
                        "expected an array of {} arguments or an object with keys [{}]",
                        names.len(),
                        names.join(", ")
                    ),
                ));
            }
        };
        Ok(Self {
            path,
            names,
            values: values.into_iter(),
            index: 0,
        })
    }

    /// Decode the next argument in declaration order.
    pub fn next_arg<T: DeserializeOwned>(&mut self) -> StructResult<T> {
        let name = self.names.get(self.index).copied().unwrap_or("?");
        self.index += 1;
        let value = self.values.next().unwrap_or(Value::Null);
        serde_json::from_value(value).map_err(|source| StructError::Deserialize {
            path: format!("{path}({name})", path = self.path),
            source,
        })
    }
}

/// A `StructError::Deserialize` carrying a message of our own rather than one
/// from serde, so argument-shape complaints read like every other body error.
fn arg_error(path: &str, message: String) -> StructError {
    StructError::Deserialize {
        path: path.to_string(),
        source: <serde_json::Error as serde::de::Error>::custom(message),
    }
}
