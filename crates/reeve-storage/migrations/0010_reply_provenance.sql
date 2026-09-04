-- A dispatched metric records what became of it but never recorded what
-- it was shown. The judge grades one turn's worth of replies and the
-- rule choosing between them lives in the capture reader, so a verdict
-- computed on a preamble and a verdict computed on a whole turn have
-- been writing identical rows.
--
-- That matters most on the outcomes that carry no score. Refusing to
-- find a claim in 147 characters of "on it" is correct behaviour and
-- indistinguishable, after the fact, from refusing to find one in a
-- turn that made a dozen checkable assertions.
--
-- Null on every row written before this, and on any trace off the SDK
-- path, which carries its reply on the span and has no rounds to
-- choose between.
ALTER TABLE judge_attempts ADD COLUMN reply_chars_shown     INTEGER;
ALTER TABLE judge_attempts ADD COLUMN reply_chars_available INTEGER;
ALTER TABLE judge_attempts ADD COLUMN reply_index           INTEGER;
ALTER TABLE judge_attempts ADD COLUMN replies_available     INTEGER;
