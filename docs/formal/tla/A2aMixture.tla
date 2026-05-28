---- MODULE A2aMixture ----
\* Mixture Style (RecursiveMAS Table 1): O↔Sp1, O↔Sp2, O↔Sp3, O↔Summarizer.
\* A fixed-NStages instance of the generic A2aLinearPipeline.
EXTENDS Naturals
VARIABLE step
INSTANCE A2aLinearPipeline WITH NStages <- 4
====
