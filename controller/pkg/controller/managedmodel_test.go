package controller

import (
	"context"
	"slices"
	"strings"
	"testing"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/meta"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"
	gwv1 "sigs.k8s.io/gateway-api/apis/v1"

	"github.com/agentgateway/agentgateway/controller/api/v1alpha1/agentgateway"
	"github.com/agentgateway/agentgateway/controller/pkg/utils/kubeutils"
)

func TestManagedModelReconcileCreatesManagedWorkload(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	scheme := managedModelTestScheme(t)
	replicas, parallelism := int32(2), int32(4)
	provider := agentgateway.ModelProviderManaged
	match := agentgateway.LongString("qwen-chat")
	model := &agentgateway.AgentgatewayModel{
		ObjectMeta: metav1.ObjectMeta{Name: "qwen", Namespace: "default", Generation: 3, UID: "model-uid"},
		Spec: agentgateway.AgentgatewayModelSpec{
			ParentRefs: []gwv1.ParentReference{{Name: "inference-gateway"}},
			Match:      &agentgateway.ModelMatch{Model: &match},
			Provider:   &provider,
			Managed: &agentgateway.ManagedModelSettings{
				ModelURI:    "hf://Qwen/Qwen3-8B-Instruct@revision-1",
				Replicas:    &replicas,
				Parallelism: &parallelism,
			},
		},
	}
	cli := fake.NewClientBuilder().
		WithScheme(scheme).
		WithStatusSubresource(&agentgateway.AgentgatewayModel{}, &appsv1.Deployment{}).
		WithObjects(model).
		Build()
	r := &managedModelReconciler{Client: cli, scheme: scheme}
	key := types.NamespacedName{Namespace: "default", Name: "qwen"}

	if _, err := r.Reconcile(ctx, ctrl.Request{NamespacedName: key}); err != nil {
		t.Fatalf("reconcile failed: %v", err)
	}

	workloadName := kubeutils.ManagedModelWorkloadName(model.Name)
	deployment := &appsv1.Deployment{}
	if err := cli.Get(ctx, types.NamespacedName{Namespace: "default", Name: workloadName}, deployment); err != nil {
		t.Fatalf("get Deployment: %v", err)
	}
	container := deployment.Spec.Template.Spec.Containers[0]
	wantArgs := []string{
		"-c", rustFrontendLauncher, "vllm-rs", "serve",
		"Qwen/Qwen3-8B-Instruct",
		"--served-model-name", "qwen-chat",
		"--tensor-parallel-size", "4",
		"--enable-prefix-caching",
		"--host", "0.0.0.0",
		"--port", "8000",
		"--grpc-port", "50051",
		"--revision", "revision-1",
	}
	if deployment.Spec.Replicas == nil || *deployment.Spec.Replicas != 2 ||
		container.Image != defaultVLLMImage ||
		!slices.Equal(container.Command, []string{"python3"}) ||
		!slices.Equal(container.Args, wantArgs) {
		t.Fatalf("unexpected managed Deployment: replicas=%v container=%+v", deployment.Spec.Replicas, container)
	}
	if got := container.Resources.Limits[corev1.ResourceName("nvidia.com/gpu")]; got.Cmp(resource.MustParse("4")) != 0 {
		t.Fatalf("expected four GPUs, got %s", got.String())
	}
	if owner := metav1.GetControllerOf(deployment); owner == nil || owner.Kind != "AgentgatewayModel" || owner.Name != "qwen" {
		t.Fatalf("unexpected Deployment owner: %v", owner)
	}

	service := &corev1.Service{}
	if err := cli.Get(ctx, types.NamespacedName{Namespace: "default", Name: workloadName}, service); err != nil {
		t.Fatalf("get Service: %v", err)
	}
	if len(service.Spec.Ports) != 2 || service.Spec.Ports[1].Port != vllmGRPCPort ||
		service.Spec.Ports[1].AppProtocol == nil || *service.Spec.Ports[1].AppProtocol != "kubernetes.io/h2c" {
		t.Fatalf("unexpected managed Service ports: %v", service.Spec.Ports)
	}

	updated := &agentgateway.AgentgatewayModel{}
	if err := cli.Get(ctx, key, updated); err != nil {
		t.Fatal(err)
	}
	ready := meta.FindStatusCondition(updated.Status.Conditions, modelConditionWorkloadReady)
	if updated.Status.Replicas != 2 || ready == nil || ready.Reason != "ReplicasNotReady" {
		t.Fatalf("unexpected model workload status: %+v", updated.Status)
	}
}

func TestManagedModelUsesExplicitHuggingFaceToken(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	scheme := managedModelTestScheme(t)
	provider := agentgateway.ModelProviderManaged
	model := &agentgateway.AgentgatewayModel{
		ObjectMeta: metav1.ObjectMeta{Name: "private", Namespace: "default", UID: "model-uid"},
		Spec: agentgateway.AgentgatewayModelSpec{
			ParentRefs: []gwv1.ParentReference{{Name: "gateway"}},
			Provider:   &provider,
			Managed: &agentgateway.ManagedModelSettings{
				ModelURI:       "hf://org/private",
				TokenSecretRef: &agentgateway.LocalSecretKeyRef{Name: "hf-token"},
			},
		},
	}
	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: "hf-token", Namespace: "default"},
		Data:       map[string][]byte{"token": []byte("secret")},
	}
	cli := fake.NewClientBuilder().
		WithScheme(scheme).
		WithStatusSubresource(&agentgateway.AgentgatewayModel{}).
		WithObjects(model, secret).
		Build()
	r := &managedModelReconciler{Client: cli, scheme: scheme}
	if _, err := r.Reconcile(ctx, ctrl.Request{NamespacedName: types.NamespacedName{Namespace: "default", Name: "private"}}); err != nil {
		t.Fatal(err)
	}
	deployment := &appsv1.Deployment{}
	if err := cli.Get(ctx, types.NamespacedName{Namespace: "default", Name: "private-llm"}, deployment); err != nil {
		t.Fatal(err)
	}
	for _, env := range deployment.Spec.Template.Spec.Containers[0].Env {
		if env.Name == "HF_TOKEN" && env.ValueFrom != nil && env.ValueFrom.SecretKeyRef != nil &&
			env.ValueFrom.SecretKeyRef.Name == "hf-token" && env.ValueFrom.SecretKeyRef.Key == "token" {
			return
		}
	}
	t.Fatal("HF_TOKEN was not configured from the explicit Secret reference")
}

func TestParseHuggingFaceURI(t *testing.T) {
	t.Parallel()
	repository, revision, err := parseHuggingFaceURI("hf://org/model@refs/pr/1")
	if err != nil || repository != "org/model" || revision != "refs/pr/1" {
		t.Fatalf("unexpected parse result: repository=%q revision=%q err=%v", repository, revision, err)
	}
	for _, uri := range []string{"org/model", "hf://model", "hf://org/model@"} {
		if _, _, err := parseHuggingFaceURI(uri); err == nil {
			t.Fatalf("expected %q to fail", uri)
		}
	}
}

func TestManagedModelWorkloadNameIsStableAndBounded(t *testing.T) {
	t.Parallel()
	long := strings.Repeat("a", 100)
	first := kubeutils.ManagedModelWorkloadName(long)
	if len(first) > 63 || first != kubeutils.ManagedModelWorkloadName(long) || !strings.HasSuffix(first, "-llm") {
		t.Fatalf("unexpected generated name %q", first)
	}
}

func managedModelTestScheme(t *testing.T) *runtime.Scheme {
	t.Helper()
	scheme := runtime.NewScheme()
	for name, add := range map[string]func(*runtime.Scheme) error{
		"core":         corev1.AddToScheme,
		"apps":         appsv1.AddToScheme,
		"gateway":      gwv1.Install,
		"agentgateway": agentgateway.Install,
	} {
		if err := add(scheme); err != nil {
			t.Fatalf("add %s scheme: %v", name, err)
		}
	}
	return scheme
}
