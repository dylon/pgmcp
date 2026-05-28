---- MODULE A2aDistillation ----
\* Distillation Style (RecursiveMAS Table 1): O↔Expert, O↔Learner.
\* A fixed-NStages instance of the generic A2aLinearPipeline.
EXTENDS Naturals
VARIABLE step
INSTANCE A2aLinearPipeline WITH NStages <- 2
====
