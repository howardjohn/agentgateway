package nack

import (
	"encoding/json"
	"strings"
	"sync"
	"time"

	"istio.io/istio/pkg/config/schema/gvr"
	"istio.io/istio/pkg/kube"
	"istio.io/istio/pkg/kube/kclient"
	"istio.io/istio/pkg/kube/krt"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	typedcorev1 "k8s.io/client-go/kubernetes/typed/core/v1"
	"k8s.io/client-go/tools/record"
	gwv1 "sigs.k8s.io/gateway-api/apis/v1"

	"github.com/agentgateway/agentgateway/api"
	agwir "github.com/agentgateway/agentgateway/controller/pkg/agentgateway/ir"
	"github.com/agentgateway/agentgateway/controller/pkg/agentgateway/plugins"
	"github.com/agentgateway/agentgateway/controller/pkg/logging"
	"github.com/agentgateway/agentgateway/controller/pkg/schemes"
	"github.com/agentgateway/agentgateway/controller/pkg/wellknown"
)

var log = logging.New("nack/publisher")

// Event reasons for Kubernetes Events created by agentgateway NACK detection
const (
	ReasonNack = "AgentGatewayNackError"
)

// NackEvent represents a NACK received from an agentgateway gateway
type NackEvent struct {
	Gateway   types.NamespacedName
	TypeUrl   string
	Nonce     string
	ErrorMsg  string
	Timestamp time.Time
}

// Diagnostic is a structured agentgateway NACK diagnostic.
type Diagnostic struct {
	Key   string `json:"key"`
	Warn  string `json:"warn,omitempty"`
	Error string `json:"error,omitempty"`
}

func ParseDiagnostics(message string) ([]Diagnostic, bool) {
	if !strings.HasPrefix(strings.TrimSpace(message), "[") {
		return nil, false
	}

	var diagnostics []Diagnostic
	if err := json.Unmarshal([]byte(message), &diagnostics); err != nil || len(diagnostics) == 0 {
		return nil, false
	}

	for _, diagnostic := range diagnostics {
		if diagnostic.Key == "" {
			return nil, false
		}
		if diagnostic.Warn == "" && diagnostic.Error == "" {
			return nil, false
		}
	}

	return diagnostics, true
}

// Publisher converts NACK events from the agentgateway xDS server into Kubernetes Events.
type Publisher struct {
	eventRecorder    record.EventRecorder
	gatewayClient    kclient.Client[*gwv1.Gateway]
	deploymentClient kclient.Client[*appsv1.Deployment]
	resourceResolver func(gateway types.NamespacedName, key string) *corev1.ObjectReference
	resourceNacks    krt.StaticCollection[plugins.ResourceNack]
	ackedMu          sync.Mutex
	sentResources    map[ackedResponse][]string
	HasSynced        func() bool
}

type ackedResponse struct {
	Gateway types.NamespacedName
	TypeUrl string
	Nonce   string
}

// NewPublisher creates a new NACK event publisher that will publish k8s events
func NewPublisher(client kube.Client) *Publisher {
	eventBroadcaster := record.NewBroadcaster()
	eventRecorder := eventBroadcaster.NewRecorder(
		schemes.DefaultScheme(),
		corev1.EventSource{Component: wellknown.DefaultAgwControllerName},
	)
	eventBroadcaster.StartRecordingToSink(&typedcorev1.EventSinkImpl{
		Interface: client.Kube().CoreV1().Events(""),
	})

	filter := kclient.Filter{ObjectFilter: client.ObjectFilter()}
	gatewayClient := kclient.NewFilteredDelayed[*gwv1.Gateway](client, gvr.KubernetesGateway, filter)
	deploymentClient := kclient.NewFiltered[*appsv1.Deployment](client, filter)
	return &Publisher{
		eventRecorder:    eventRecorder,
		gatewayClient:    gatewayClient,
		deploymentClient: deploymentClient,
		sentResources:    map[ackedResponse][]string{},
		HasSynced: func() bool {
			return gatewayClient.HasSynced() && deploymentClient.HasSynced()
		},
	}
}

// SetResourceCollection configures resource-key resolution for structured NACK
// diagnostics. This lets NACK keys like "policy/..." map back to their source
// Kubernetes object.
func (p *Publisher) SetResourceCollection(resources krt.Collection[agwir.AgwResource]) {
	p.resourceResolver = func(gateway types.NamespacedName, key string) *corev1.ObjectReference {
		if resources == nil {
			return nil
		}
		resource := resources.GetKey(gateway.String() + "/" + key)
		if resource == nil {
			resource = resources.GetKey(types.NamespacedName{}.String() + "/" + key)
		}
		if resource == nil {
			return nil
		}
		return objectReferenceForResource(resource.Resource)
	}
	prevHasSynced := p.HasSynced
	p.HasSynced = func() bool {
		return (prevHasSynced == nil || prevHasSynced()) && resources.HasSynced()
	}
}

func (p *Publisher) SetNackCollection(resourceNacks krt.StaticCollection[plugins.ResourceNack]) {
	p.resourceNacks = resourceNacks
}

func (p *Publisher) RecordSentResources(gateway types.NamespacedName, typeURL string, nonce string, resourceNames []string) {
	if nonce == "" || len(resourceNames) == 0 {
		return
	}
	p.ackedMu.Lock()
	defer p.ackedMu.Unlock()
	if p.sentResources == nil {
		p.sentResources = map[ackedResponse][]string{}
	}
	p.sentResources[ackedResponse{Gateway: gateway, TypeUrl: typeURL, Nonce: nonce}] = resourceNames
}

func (p *Publisher) PublishAck(gateway types.NamespacedName, typeURL string, nonce string) {
	if nonce == "" || p.resourceNacks == (krt.StaticCollection[plugins.ResourceNack]{}) {
		return
	}
	key := ackedResponse{Gateway: gateway, TypeUrl: typeURL, Nonce: nonce}
	p.ackedMu.Lock()
	resourceNames := p.sentResources[key]
	delete(p.sentResources, key)
	p.ackedMu.Unlock()
	for _, resourceName := range resourceNames {
		p.resourceNacks.DeleteObject(gateway.String() + "/" + resourceName)
	}
}

// PublishNack publishes a NACK event as a k8s event.
func (p *Publisher) PublishNack(event *NackEvent) {
	defer p.forgetSentResources(event.Gateway, event.TypeUrl, event.Nonce)
	if p.publishResourceNacks(event) {
		return
	}
	p.publishGatewayNack(event)
}

func (p *Publisher) publishResourceNacks(event *NackEvent) bool {
	if p.resourceResolver == nil {
		return false
	}
	diagnostics, ok := ParseDiagnostics(event.ErrorMsg)
	if !ok {
		return false
	}

	published := false
	seen := map[corev1.ObjectReference]struct{}{}
	for _, diagnostic := range diagnostics {
		ref := p.resourceResolver(event.Gateway, diagnostic.Key)
		if ref == nil {
			continue
		}
		p.publishResourceNackStatus(event, diagnostic)
		if _, f := seen[*ref]; f {
			continue
		}
		seen[*ref] = struct{}{}
		p.eventRecorder.Event(ref, corev1.EventTypeWarning, ReasonNack, event.ErrorMsg)
		published = true
	}
	if published {
		log.Debug("published NACK event for resource", "gateway", event.Gateway, "typeURL", event.TypeUrl)
	}
	return published
}

func (p *Publisher) forgetSentResources(gateway types.NamespacedName, typeURL string, nonce string) {
	if nonce == "" {
		return
	}
	p.ackedMu.Lock()
	delete(p.sentResources, ackedResponse{Gateway: gateway, TypeUrl: typeURL, Nonce: nonce})
	p.ackedMu.Unlock()
}

func (p *Publisher) publishResourceNackStatus(event *NackEvent, diagnostic Diagnostic) {
	if p.resourceNacks == (krt.StaticCollection[plugins.ResourceNack]{}) {
		return
	}
	message := diagnostic.Error
	if message == "" {
		message = diagnostic.Warn
	}
	p.resourceNacks.UpdateObject(plugins.ResourceNack{
		Gateway:   event.Gateway,
		Key:       diagnostic.Key,
		TypeUrl:   event.TypeUrl,
		Message:   message,
		Timestamp: event.Timestamp,
	})
}

func (p *Publisher) publishGatewayNack(event *NackEvent) {
	var gatewayUID, deployUID types.UID
	gw := p.gatewayClient.Get(event.Gateway.Name, event.Gateway.Namespace)
	if gw == nil {
		log.Error("failed to get gateway from cache")
		return
	}
	gatewayUID = gw.GetUID()
	dep := p.deploymentClient.Get(event.Gateway.Name, event.Gateway.Namespace)
	if dep == nil {
		log.Error("failed to get deployment from cache")
		return
	}
	deployUID = dep.GetUID()

	gatewayRef := &corev1.ObjectReference{
		Kind:       wellknown.GatewayKind,
		APIVersion: wellknown.GatewayGVK.GroupVersion().String(),
		Name:       event.Gateway.Name,
		Namespace:  event.Gateway.Namespace,
		UID:        gatewayUID,
	}
	deploymentRef := &corev1.ObjectReference{
		Kind:       wellknown.DeploymentGVK.Kind,
		APIVersion: wellknown.DeploymentGVK.GroupVersion().String(),
		Name:       event.Gateway.Name,
		Namespace:  event.Gateway.Namespace,
		UID:        deployUID,
	}

	p.eventRecorder.Event(gatewayRef, corev1.EventTypeWarning, ReasonNack, event.ErrorMsg)
	p.eventRecorder.Event(deploymentRef, corev1.EventTypeWarning, ReasonNack, event.ErrorMsg)

	log.Debug("published NACK event for Gateway", "gateway", event.Gateway, "typeURL", event.TypeUrl)
}

func objectReferenceForResource(resource *api.Resource) *corev1.ObjectReference {
	if resource == nil {
		return nil
	}
	policy := resource.GetPolicy()
	if policy == nil || policy.Name == nil {
		return nil
	}
	gvk, ok := gvkForKind(policy.Name.Kind)
	if !ok {
		return nil
	}
	return &corev1.ObjectReference{
		Kind:       policy.Name.Kind,
		APIVersion: gvk.GroupVersion().String(),
		Name:       policy.Name.Name,
		Namespace:  policy.Name.Namespace,
	}
}

func gvkForKind(kind string) (schema.GroupVersionKind, bool) {
	switch kind {
	case wellknown.AgentgatewayPolicyGVK.Kind:
		return wellknown.AgentgatewayPolicyGVK, true
	case wellknown.ServiceGVK.Kind:
		return wellknown.ServiceGVK, true
	case wellknown.SecretGVK.Kind:
		return wellknown.SecretGVK, true
	case wellknown.ConfigMapGVK.Kind:
		return wellknown.ConfigMapGVK, true
	default:
		gvk, ok := wellknown.KnownGvkByKind[kind]
		return gvk, ok
	}
}
