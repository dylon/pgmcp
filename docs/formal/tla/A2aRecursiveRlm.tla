---- MODULE A2aRecursiveRlm ----
\* Recursive / RLM Style (RecursiveMAS Section 5): O↔Sub1 … O↔Sub_D, the
\* unrolled depth-bounded self-calls (here D = 2). A fixed-NStages instance of
\* the generic A2aLinearPipeline. The unbounded ∀-depth termination is Rocq T1.
EXTENDS Naturals
VARIABLE step
INSTANCE A2aLinearPipeline WITH NStages <- 2
====
