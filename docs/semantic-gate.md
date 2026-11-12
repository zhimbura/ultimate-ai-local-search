# Semantic backend gate

The rule was fixed before the measurement. claude-context becomes the default semantic backend only
if fused search with it beats fused search with Semble on natural-language queries of half B, and
the lower bound of the 95% confidence interval of the MRR difference is above 0. Otherwise Semble
stays the default, because it is simpler and needs no Docker.

**Result: Semble stays the default.** The default fusion weights do not change.

## Setup

- Gold set: 80 queries over a private workspace of 4 repositories. The set is not in this
  repository. It has 4 query types (identifier, natural, cross_repo, config), 2 halves (A and B),
  and 10 queries of each type in each half. The queries were written before anyone looked at the
  backend answers. Natural queries contain no identifiers.
- Fusion: every backend gives one list over all repositories (`fuse`, rule 2). Semble searches all
  repositories in one call. claude-context gives no scores, so its lists of the repositories
  interleave by rank.
- Weights were picked on half A. Natural queries: semantic {0.35, 0.7, 1.0, 1.4} × lexical
  {0, 0.35, 0.7}, with ast-index at 0.7. Identifier queries: semantic {0, 0.15, 0.3, 0.5}.
- The report uses half B. `uals-bench compare` gives a paired bootstrap with 10 000 resamples and a
  95% confidence interval.

## Gate result (half B)

MRR of the first expected file. The semantic columns use the weights picked on half A.

| type | n | no semantics | Semble | claude-context |
|---|---:|---:|---:|---:|
| identifier | 10 | 1.000 | 0.950 | 0.950 |
| natural | 10 | 0.053 | 0.008 | 0.021 |
| cross_repo | 10 | 0.361 | 0.392 | 0.317 |
| config | 10 | 0.000 | 0.013 | 0.000 |
| all | 40 | 0.354 | 0.341 | 0.322 |

claude-context minus Semble on natural queries: +0.013, 95% CI [−0.015, +0.046]. The lower bound is
below 0, so the rule keeps Semble.

## Weights

- Picks on half A: for Semble, natural lexical 0.7 (natural semantic 1.0 is the default); for
  claude-context, natural semantic 0.35. No identifier semantic weight changed any result.
- On half B the picks do not beat the defaults. Picked minus default: Semble −0.001,
  95% CI [−0.061, +0.052]; claude-context +0.022, 95% CI [−0.025, +0.071].
- So the defaults stay. The plan adds lexical search to natural queries only if the measurement
  shows a gain.

## Notes

- One list per backend helps cross-repository queries. All 40 queries of half B, before and after
  the change, default weights: no semantics 0.336 → 0.354, Semble 0.312 → 0.342,
  claude-context 0.311 → 0.300.
- On this set no semantic backend beats fused search without semantics. claude-context with its
  picked weights is below it over all 40 queries: −0.032, 95% CI [−0.071, −0.001].
- Every configuration misses most natural queries: the semantic backends seldom return the
  expected files.
