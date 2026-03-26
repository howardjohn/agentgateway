package deployer

import (
	"github.com/agentgateway/agentgateway/controller/api/v1alpha1/agentgateway"
	agwplugins "github.com/agentgateway/agentgateway/controller/pkg/agentgateway/plugins"
)

// Inputs is the set of options used to configure gateway/inference pool deployment.
type Inputs struct {
	Dev                        bool
	ImageDefaults              *agentgateway.Image
	ControlPlane               ControlPlaneInfo
	NoListenersDummyPort       uint16
	ImageInfo                  *ImageInfo
	AgwCollections             *agwplugins.AgwCollections
	AgentgatewayClassName      string
	AgentgatewayControllerName string
}

// InMemoryGatewayParametersConfig holds the configuration for creating in-memory GatewayParameters.
type InMemoryGatewayParametersConfig struct {
	ClassName                  string
	ImageInfo                  *ImageInfo
	AgwControllerName          string
	OmitDefaultSecurityContext bool
}
