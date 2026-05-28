---- MODULE A2aSequential ----
\* Sequential Style (RecursiveMAS Table 1): O↔Planner, O↔Critic, O↔Solver.
\* A fixed-NStages instance of the generic A2aLinearPipeline.
EXTENDS Naturals
VARIABLE step
INSTANCE A2aLinearPipeline WITH NStages <- 3
====
