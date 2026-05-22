package nack

import (
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"istio.io/istio/pkg/kube/krt"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/client-go/tools/record"
	gwv1 "sigs.k8s.io/gateway-api/apis/v1"

	"github.com/agentgateway/agentgateway/api"
	agwir "github.com/agentgateway/agentgateway/controller/pkg/agentgateway/ir"
	"github.com/agentgateway/agentgateway/controller/pkg/agentgateway/plugins"
	"github.com/agentgateway/agentgateway/controller/pkg/apiclient/fake"
	"github.com/agentgateway/agentgateway/controller/pkg/wellknown"
)

var (
	testGateway      = types.NamespacedName{Name: "test-gw", Namespace: "default"}
	testTypeURL      = "type.googleapis.com/agentgateway.dev.resource.Resource"
	testErrorMessage = "test error"
	testNackEvent    = NackEvent{
		Gateway:   testGateway,
		TypeUrl:   testTypeURL,
		ErrorMsg:  testErrorMessage,
		Timestamp: time.Now(),
	}
)

type capturedEvent struct {
	object  runtime.Object
	reason  string
	message string
}

type capturingRecorder struct {
	events []capturedEvent
}

func (r *capturingRecorder) Event(object runtime.Object, eventtype, reason, message string) {
	r.events = append(r.events, capturedEvent{object: object, reason: reason, message: message})
}

func (r *capturingRecorder) Eventf(object runtime.Object, eventtype, reason, messageFmt string, args ...any) {
	r.Event(object, eventtype, reason, messageFmt)
}

func (r *capturingRecorder) AnnotatedEventf(object runtime.Object, annotations map[string]string, eventtype, reason, messageFmt string, args ...any) {
	r.Event(object, eventtype, reason, messageFmt)
}

func TestPublisher_PublishNack(t *testing.T) {
	ctx := t.Context()

	// Ensure involved objects exist so UID lookups succeed
	gw := &gwv1.Gateway{
		ObjectMeta: metav1.ObjectMeta{
			Name:      testGateway.Name,
			Namespace: testGateway.Namespace,
		},
	}
	dep := &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{
			Name:      testGateway.Name,
			Namespace: testGateway.Namespace,
		},
	}

	fakeClient := fake.NewClient(t, gw, dep)

	publisher := NewPublisher(fakeClient)
	fakeRecorder := record.NewFakeRecorder(10)
	publisher.eventRecorder = fakeRecorder

	fakeClient.RunAndWait(ctx.Done())

	fakeClient.WaitForCacheSync("test-publisher", ctx.Done(), publisher.HasSynced)

	publisher.PublishNack(&testNackEvent)

	select {
	case event := <-fakeRecorder.Events:
		assert.Contains(t, event, "Warning")
		assert.Contains(t, event, ReasonNack)
		assert.Contains(t, event, testErrorMessage)
	default:
		t.Fatal("Expected event to be recorded but none was found")
	}
}

func TestPublisher_PublishNackOnResourceFromStructuredDiagnostic(t *testing.T) {
	ctx := t.Context()
	gw := &gwv1.Gateway{
		ObjectMeta: metav1.ObjectMeta{
			Name:      testGateway.Name,
			Namespace: testGateway.Namespace,
		},
	}
	dep := &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{
			Name:      testGateway.Name,
			Namespace: testGateway.Namespace,
		},
	}

	fakeClient := fake.NewClient(t, gw, dep)
	publisher := NewPublisher(fakeClient)
	recorder := &capturingRecorder{}
	publisher.eventRecorder = recorder
	nackStatus := krt.NewStaticCollection[plugins.ResourceNack](nil, nil, krt.WithName("test/Nacks"))
	publisher.SetNackCollection(nackStatus)

	resourceKey := "policy/traffic/default/ext-api-jwt:jwt:default/gateway"
	resources := krt.NewStaticCollection[agwir.AgwResource](nil, []agwir.AgwResource{{
		Gateway: testGateway,
		Resource: &api.Resource{
			Kind: &api.Resource_Policy{
				Policy: &api.Policy{
					Key: "traffic/default/ext-api-jwt:jwt:default/gateway",
					Name: &api.TypedResourceName{
						Kind:      wellknown.AgentgatewayPolicyGVK.Kind,
						Namespace: "default",
						Name:      "ext-api-jwt",
					},
				},
			},
		},
	}}, krt.WithName("test/Resources"))
	publisher.SetResourceCollection(resources)

	fakeClient.RunAndWait(ctx.Done())
	fakeClient.WaitForCacheSync("test-publisher", ctx.Done(), publisher.HasSynced)

	msg := `[{"key":"` + resourceKey + `","error":"error: failed to create JWT config: the key is missing the kid attribute"}]`
	publisher.PublishNack(&NackEvent{
		Gateway:   testGateway,
		TypeUrl:   testTypeURL,
		ErrorMsg:  msg,
		Timestamp: time.Now(),
	})

	if len(recorder.events) != 1 {
		t.Fatalf("expected one resource event, got %d", len(recorder.events))
	}
	assert.Equal(t, ReasonNack, recorder.events[0].reason)
	assert.Equal(t, msg, recorder.events[0].message)
	ref, ok := recorder.events[0].object.(*corev1.ObjectReference)
	if !ok {
		t.Fatalf("expected ObjectReference, got %T", recorder.events[0].object)
	}
	assert.Equal(t, wellknown.AgentgatewayPolicyGVK.Kind, ref.Kind)
	assert.Equal(t, wellknown.AgentgatewayPolicyGVK.GroupVersion().String(), ref.APIVersion)
	assert.Equal(t, "default", ref.Namespace)
	assert.Equal(t, "ext-api-jwt", ref.Name)

	nackKey := testGateway.String() + "/" + resourceKey
	stored := nackStatus.GetKey(nackKey)
	if stored == nil {
		t.Fatalf("expected NACK status for %s", nackKey)
	}
	assert.Equal(t, "error: failed to create JWT config: the key is missing the kid attribute", stored.Message)

	publisher.RecordSentResources(testGateway, testTypeURL, "nonce-1", []string{resourceKey})
	publisher.PublishAck(testGateway, testTypeURL, "nonce-1")
	assert.Nil(t, nackStatus.GetKey(nackKey))
}
