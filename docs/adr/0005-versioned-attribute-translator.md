# 0005: Versioned AttributeTranslator Pattern

**Status:** Accepted
**Date:** 2026-06-26

## Context

The normalize stage must translate OTLP span attributes into Reeve's
internal `InternalSpan` type. The specific attributes that carry
meaningful information are defined by the OTel GenAI semantic
conventions, a specification that is explicitly marked experimental
and has changed between versions already. Any code that hardcodes the
current attribute names without isolation is a change magnet: every
time the convention updates, the translation logic changes, and those
changes touch code that was previously working correctly.

The question is how to structure the translation so that schema
evolution does not silently break spans that were already being
handled correctly.

## Decision

The normalize stage defines an `AttributeTranslator` trait. Concrete
implementations are named by the schema version they implement:
`V1AttributeTranslator` for the current OTel GenAI convention. When
the convention changes, a `V2AttributeTranslator` is added alongside
`V1` rather than replacing it. The version is encoded in the type
name, not in a runtime parameter or configuration field.

The `raw_attributes` field on `InternalSpan` is the companion to this
decision. Any attribute outside the translator's known set lands in
`raw_attributes` rather than being silently dropped. This means new
convention fields are preserved in their raw form even before a new
translator version knows how to interpret them. No data is thrown away
because the translator does not recognize it yet.

## Consequences

**What gets easier:**
- Old spans processed by `V1AttributeTranslator` continue to behave
  correctly when `V2` is introduced, because the type is never
  modified.
- Each translator version is independently testable. Test coverage
  for `V1` is not at risk when `V2` is added.
- The `raw_attributes` field gives forward compatibility without any
  code change: new convention attributes are preserved automatically.

**What gets harder:**
- If two translator versions need to coexist at runtime, the caller
  must choose which one to instantiate. For v0.1.0 this is trivial
  since there is only one version, but a migration path will need
  design when `V2` arrives.

## Alternatives considered

**Single translator updated in place (rejected):** The simpler
approach. One struct, one set of attribute mappings, update it when
the convention changes. Rejected because it creates a mutable target:
a change to handle a new attribute name might inadvertently change
how an existing name is handled, and existing tests may not catch it
if the convention shifts incrementally.

**Runtime version parameter (rejected):** Passing the schema version
as a constructor argument (`AttributeTranslator::new("v1")`) and
branching internally. Rejected because it conflates two distinct
implementations into one type and makes each branch harder to test
in isolation. The type system already provides a clean way to express
"these are different things": different types.

**No versioning, rely on `raw_attributes` alone (rejected):** If
everything unknown lands in `raw_attributes`, schema evolution is
handled implicitly for reading. Rejected because the structured
`attributes` field would then be empty until someone writes code to
populate it, which is exactly the translation work the translator is
supposed to do. Versioning and the catch-all are complementary, not
alternatives.
