-- final_health_score records the number and not the weight it was computed
-- over, so a trace judged on all five metrics and one judged on the three
-- tier 1 heuristics are indistinguishable once stored. compute() already
-- returns the coverage; this stops it being dropped at the write.
ALTER TABLE traces ADD COLUMN weight_coverage REAL;

-- Judge results are saved before the merge filters out the ones the judge
-- had low confidence in, so the row that counted and the row that was
-- discarded look the same. Nullable because the heuristics have nothing
-- to put here.
ALTER TABLE evaluation_results ADD COLUMN confidence TEXT;
