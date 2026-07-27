package kubeutils

import (
	"crypto/sha256"
	"fmt"
	"strings"
)

const ManagedModelGRPCPort = 50051

// ManagedModelWorkloadName returns the shared name of the Deployment and
// Service owned by an AgentgatewayModel.
func ManagedModelWorkloadName(modelName string) string {
	const suffix = "-llm"
	if len(modelName)+len(suffix) <= 63 {
		return modelName + suffix
	}
	hash := fmt.Sprintf("%x", sha256.Sum256([]byte(modelName)))[:8]
	return strings.TrimRight(modelName[:63-len(suffix)-len(hash)-1], "-") + "-" + hash + suffix
}
