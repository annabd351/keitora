// First-run onboarding flow: full-screen wrapper around AddProjectFlow.
// Shown by App.tsx when currentProject() returns null (no cancel affordance —
// first-run requires completing setup). The step machine lives in AddProjectFlow.

import { AddProjectFlow } from "./AddProjectFlow";

interface OnboardingProps {
  /** Called after a project is successfully registered and active. */
  onComplete: () => void;
}

export function Onboarding({ onComplete }: OnboardingProps) {
  // First-run: no cancel — the user must add a project to proceed.
  return <AddProjectFlow onComplete={onComplete} />;
}
