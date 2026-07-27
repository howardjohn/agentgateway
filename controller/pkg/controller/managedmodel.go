package controller

import (
	"context"
	"fmt"
	"reflect"
	"strconv"
	"strings"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/util/intstr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/handler"
	"sigs.k8s.io/controller-runtime/pkg/manager"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"

	"github.com/agentgateway/agentgateway/controller/api/v1alpha1/agentgateway"
	"github.com/agentgateway/agentgateway/controller/pkg/utils/kubeutils"
)

const (
	defaultVLLMImage = "vllm/vllm-openai:v0.25.1"
	vllmHTTPPort     = 8000
	vllmGRPCPort     = kubeutils.ManagedModelGRPCPort

	// The official image packages vllm-rs beside the Python package but does
	// not install it on PATH.
	rustFrontendLauncher = `import importlib.util,os,sys;p=os.path.join(os.path.dirname(importlib.util.find_spec("vllm").origin),"vllm-rs");os.execv(p,[p,*sys.argv[1:]])`

	managedModelLabel           = "agentgateway.dev/model"
	modelConditionWorkloadReady = "WorkloadReady"
	modelConditionAccepted      = "Accepted"
	managedModelControllerName  = "managed-model"
	defaultHuggingFaceSecretKey = "token"
)

type managedModelReconciler struct {
	client.Client
	scheme *runtime.Scheme
}

// SetupManagedModelController provisions the workload owned by an
// AgentgatewayModel whose provider is Managed.
func SetupManagedModelController(mgr manager.Manager) error {
	r := &managedModelReconciler{
		Client: mgr.GetClient(),
		scheme: mgr.GetScheme(),
	}
	return ctrl.NewControllerManagedBy(mgr).
		Named(managedModelControllerName).
		For(&agentgateway.AgentgatewayModel{}).
		Owns(&appsv1.Deployment{}).
		Owns(&corev1.Service{}).
		Watches(&corev1.Secret{}, handler.EnqueueRequestsFromMapFunc(r.requestsForSecret)).
		Complete(r)
}

func (r *managedModelReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	model := &agentgateway.AgentgatewayModel{}
	if err := r.Get(ctx, req.NamespacedName, model); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	if model.Spec.Provider == nil || *model.Spec.Provider != agentgateway.ModelProviderManaged {
		if err := r.cleanupManagedWorkload(ctx, model); err != nil {
			return ctrl.Result{}, err
		}
		model.Status.Replicas = 0
		model.Status.ReadyReplicas = 0
		meta.RemoveStatusCondition(&model.Status.Conditions, modelConditionAccepted)
		meta.RemoveStatusCondition(&model.Status.Conditions, modelConditionWorkloadReady)
		return ctrl.Result{}, r.updateModelStatus(ctx, model)
	}
	if model.Spec.Managed == nil {
		setModelCondition(model, modelConditionAccepted, metav1.ConditionFalse, "InvalidConfiguration", "provider Managed requires managed configuration")
		return ctrl.Result{}, r.updateModelStatus(ctx, model)
	}

	managed := model.Spec.Managed
	replicas := valueOrDefault(managed.Replicas, 1)
	parallelism := valueOrDefault(managed.Parallelism, 1)
	model.Status.Replicas = replicas
	servedModelName := model.Name
	if model.Spec.Match != nil && model.Spec.Match.Model != nil {
		servedModelName = string(*model.Spec.Match.Model)
	}

	repository, revision, err := parseHuggingFaceURI(managed.ModelURI)
	if err != nil {
		setModelCondition(model, modelConditionAccepted, metav1.ConditionFalse, "InvalidModelURI", err.Error())
		setModelCondition(model, modelConditionWorkloadReady, metav1.ConditionFalse, "ValidationFailed", "The managed model configuration is invalid")
		return ctrl.Result{}, r.updateModelStatus(ctx, model)
	}
	if err := r.validateTokenSecret(ctx, model); err != nil {
		setModelCondition(model, modelConditionAccepted, metav1.ConditionFalse, "TokenSecretInvalid", err.Error())
		setModelCondition(model, modelConditionWorkloadReady, metav1.ConditionFalse, "ValidationFailed", "The Hugging Face token reference is invalid")
		return ctrl.Result{}, r.updateModelStatus(ctx, model)
	}
	setModelCondition(model, modelConditionAccepted, metav1.ConditionTrue, "Accepted", "The managed model configuration is valid")

	deployment := desiredModelDeployment(model, repository, revision, servedModelName, replicas, parallelism)
	if _, err := controllerutil.CreateOrUpdate(ctx, r.Client, deployment, func() error {
		desired := desiredModelDeployment(model, repository, revision, servedModelName, replicas, parallelism)
		deployment.Labels = desired.Labels
		deployment.Annotations = desired.Annotations
		deployment.Spec = desired.Spec
		return controllerutil.SetControllerReference(model, deployment, r.scheme)
	}); err != nil {
		setModelCondition(model, modelConditionWorkloadReady, metav1.ConditionFalse, "ReconcileFailed", fmt.Sprintf("Failed to reconcile the vLLM Deployment: %v", err))
		_ = r.updateModelStatus(ctx, model)
		return ctrl.Result{}, err
	}

	service := desiredModelService(model)
	if _, err := controllerutil.CreateOrUpdate(ctx, r.Client, service, func() error {
		desired := desiredModelService(model)
		service.Labels = desired.Labels
		service.Annotations = desired.Annotations
		service.Spec.Ports = desired.Spec.Ports
		service.Spec.Selector = desired.Spec.Selector
		return controllerutil.SetControllerReference(model, service, r.scheme)
	}); err != nil {
		setModelCondition(model, modelConditionWorkloadReady, metav1.ConditionFalse, "ReconcileFailed", fmt.Sprintf("Failed to reconcile the model Service: %v", err))
		_ = r.updateModelStatus(ctx, model)
		return ctrl.Result{}, err
	}

	model.Status.ReadyReplicas = deployment.Status.ReadyReplicas
	if deployment.Status.ObservedGeneration >= deployment.Generation && deployment.Status.ReadyReplicas == replicas {
		setModelCondition(model, modelConditionWorkloadReady, metav1.ConditionTrue, "Ready", "All managed vLLM replicas are ready")
	} else {
		setModelCondition(model, modelConditionWorkloadReady, metav1.ConditionFalse, "ReplicasNotReady", fmt.Sprintf("%d of %d managed vLLM replicas are ready", deployment.Status.ReadyReplicas, replicas))
	}
	return ctrl.Result{}, r.updateModelStatus(ctx, model)
}

func desiredModelDeployment(
	model *agentgateway.AgentgatewayModel,
	repository, revision, servedModelName string,
	replicas, parallelism int32,
) *appsv1.Deployment {
	name := kubeutils.ManagedModelWorkloadName(model.Name)
	labels := managedModelLabels(model)
	args := []string{
		"-c", rustFrontendLauncher, "vllm-rs", "serve",
		repository,
		"--served-model-name", servedModelName,
		"--tensor-parallel-size", strconv.Itoa(int(parallelism)),
		"--enable-prefix-caching",
		"--host", "0.0.0.0",
		"--port", strconv.Itoa(vllmHTTPPort),
		"--grpc-port", strconv.Itoa(vllmGRPCPort),
	}
	if revision != "" {
		args = append(args, "--revision", revision)
	}

	image := defaultVLLMImage
	if model.Spec.Managed.VLLM != nil && model.Spec.Managed.VLLM.Image != "" {
		image = model.Spec.Managed.VLLM.Image
	}
	container := corev1.Container{
		Name:    "vllm",
		Image:   image,
		Command: []string{"python3"},
		Args:    args,
		Ports: []corev1.ContainerPort{
			{Name: "http", ContainerPort: vllmHTTPPort},
			{Name: "grpc", ContainerPort: vllmGRPCPort},
		},
		Env: []corev1.EnvVar{{Name: "HF_HOME", Value: "/root/.cache/huggingface"}},
		Resources: corev1.ResourceRequirements{
			Limits: corev1.ResourceList{
				corev1.ResourceName("nvidia.com/gpu"): *resource.NewQuantity(int64(parallelism), resource.DecimalSI),
			},
		},
		VolumeMounts: []corev1.VolumeMount{
			{Name: "model-cache", MountPath: "/root/.cache/huggingface"},
			{Name: "shared-memory", MountPath: "/dev/shm"},
		},
		StartupProbe: &corev1.Probe{
			ProbeHandler:     corev1.ProbeHandler{HTTPGet: &corev1.HTTPGetAction{Path: "/health", Port: intstr.FromString("http")}},
			PeriodSeconds:    5,
			FailureThreshold: 360,
		},
		ReadinessProbe: &corev1.Probe{
			ProbeHandler:  corev1.ProbeHandler{HTTPGet: &corev1.HTTPGetAction{Path: "/health", Port: intstr.FromString("http")}},
			PeriodSeconds: 5,
		},
	}
	if token := model.Spec.Managed.TokenSecretRef; token != nil {
		key := defaultHuggingFaceSecretKey
		if token.Key != nil {
			key = *token.Key
		}
		container.Env = append(container.Env, corev1.EnvVar{
			Name: "HF_TOKEN",
			ValueFrom: &corev1.EnvVarSource{SecretKeyRef: &corev1.SecretKeySelector{
				LocalObjectReference: corev1.LocalObjectReference{Name: string(token.Name)},
				Key:                  key,
			}},
		})
	}

	terminationGracePeriodSeconds := int64(120)
	return &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: model.Namespace, Labels: labels},
		Spec: appsv1.DeploymentSpec{
			Replicas: &replicas,
			Strategy: appsv1.DeploymentStrategy{Type: appsv1.RecreateDeploymentStrategyType},
			Selector: &metav1.LabelSelector{MatchLabels: labels},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: labels},
				Spec: corev1.PodSpec{
					Containers:                    []corev1.Container{container},
					TerminationGracePeriodSeconds: &terminationGracePeriodSeconds,
					Volumes: []corev1.Volume{
						{Name: "model-cache", VolumeSource: corev1.VolumeSource{EmptyDir: &corev1.EmptyDirVolumeSource{}}},
						{Name: "shared-memory", VolumeSource: corev1.VolumeSource{EmptyDir: &corev1.EmptyDirVolumeSource{Medium: corev1.StorageMediumMemory}}},
					},
				},
			},
		},
	}
}

func desiredModelService(model *agentgateway.AgentgatewayModel) *corev1.Service {
	grpcAppProtocol := "kubernetes.io/h2c"
	return &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:      kubeutils.ManagedModelWorkloadName(model.Name),
			Namespace: model.Namespace,
			Labels:    managedModelLabels(model),
		},
		Spec: corev1.ServiceSpec{
			Selector: managedModelLabels(model),
			Ports: []corev1.ServicePort{
				{Name: "http", Port: vllmHTTPPort, TargetPort: intstr.FromString("http")},
				{Name: "grpc", Port: vllmGRPCPort, TargetPort: intstr.FromString("grpc"), AppProtocol: &grpcAppProtocol},
			},
		},
	}
}

func managedModelLabels(model *agentgateway.AgentgatewayModel) map[string]string {
	name := kubeutils.ManagedModelWorkloadName(model.Name)
	return map[string]string{
		"app.kubernetes.io/name":       "vllm",
		"app.kubernetes.io/instance":   name,
		"app.kubernetes.io/managed-by": "agentgateway",
		managedModelLabel:              name,
	}
}

func parseHuggingFaceURI(uri string) (repository, revision string, err error) {
	const prefix = "hf://"
	if !strings.HasPrefix(uri, prefix) {
		return "", "", fmt.Errorf("modelURI must start with %q", prefix)
	}
	value := strings.TrimPrefix(uri, prefix)
	repository, revision, _ = strings.Cut(value, "@")
	parts := strings.Split(repository, "/")
	if len(parts) != 2 || parts[0] == "" || parts[1] == "" {
		return "", "", fmt.Errorf("modelURI must identify a Hugging Face organization and repository")
	}
	if strings.Contains(value, "@") && revision == "" {
		return "", "", fmt.Errorf("modelURI revision must not be empty")
	}
	return repository, revision, nil
}

func (r *managedModelReconciler) validateTokenSecret(ctx context.Context, model *agentgateway.AgentgatewayModel) error {
	ref := model.Spec.Managed.TokenSecretRef
	if ref == nil {
		return nil
	}
	if ref.Group != "" || (ref.Kind != "" && ref.Kind != "Secret") {
		return fmt.Errorf("Hugging Face token reference may target only a Secret")
	}
	secret := &corev1.Secret{}
	if err := r.Get(ctx, types.NamespacedName{Namespace: model.Namespace, Name: string(ref.Name)}, secret); err != nil {
		if apierrors.IsNotFound(err) {
			return fmt.Errorf("Hugging Face token Secret %s/%s was not found", model.Namespace, ref.Name)
		}
		return fmt.Errorf("failed to read Hugging Face token Secret: %w", err)
	}
	key := defaultHuggingFaceSecretKey
	if ref.Key != nil {
		key = *ref.Key
	}
	if _, found := secret.Data[key]; !found {
		return fmt.Errorf("Hugging Face token Secret %s/%s does not contain key %q", model.Namespace, ref.Name, key)
	}
	return nil
}

func (r *managedModelReconciler) cleanupManagedWorkload(ctx context.Context, model *agentgateway.AgentgatewayModel) error {
	key := client.ObjectKey{Namespace: model.Namespace, Name: kubeutils.ManagedModelWorkloadName(model.Name)}
	for _, obj := range []client.Object{&appsv1.Deployment{}, &corev1.Service{}} {
		if err := r.Get(ctx, key, obj); err != nil {
			if apierrors.IsNotFound(err) {
				continue
			}
			return err
		}
		if owner := metav1.GetControllerOf(obj); owner != nil && owner.UID == model.UID {
			if err := r.Delete(ctx, obj); err != nil && !apierrors.IsNotFound(err) {
				return err
			}
		}
	}
	return nil
}

func (r *managedModelReconciler) updateModelStatus(ctx context.Context, model *agentgateway.AgentgatewayModel) error {
	latest := &agentgateway.AgentgatewayModel{}
	if err := r.Get(ctx, client.ObjectKeyFromObject(model), latest); err != nil {
		return client.IgnoreNotFound(err)
	}
	if latest.Status.Replicas == model.Status.Replicas &&
		latest.Status.ReadyReplicas == model.Status.ReadyReplicas &&
		reflect.DeepEqual(latest.Status.Conditions, model.Status.Conditions) {
		return nil
	}
	latest.Status.Replicas = model.Status.Replicas
	latest.Status.ReadyReplicas = model.Status.ReadyReplicas
	latest.Status.Conditions = model.Status.Conditions
	return r.Status().Update(ctx, latest)
}

func setModelCondition(model *agentgateway.AgentgatewayModel, conditionType string, status metav1.ConditionStatus, reason, message string) {
	meta.SetStatusCondition(&model.Status.Conditions, metav1.Condition{
		Type:               conditionType,
		Status:             status,
		ObservedGeneration: model.Generation,
		Reason:             reason,
		Message:            message,
	})
}

func valueOrDefault(value *int32, defaultValue int32) int32 {
	if value == nil {
		return defaultValue
	}
	return *value
}

func (r *managedModelReconciler) requestsForSecret(ctx context.Context, obj client.Object) []reconcile.Request {
	models := &agentgateway.AgentgatewayModelList{}
	if err := r.List(ctx, models, client.InNamespace(obj.GetNamespace())); err != nil {
		return nil
	}
	requests := make([]reconcile.Request, 0)
	for i := range models.Items {
		model := &models.Items[i]
		if model.Spec.Managed != nil && model.Spec.Managed.TokenSecretRef != nil &&
			string(model.Spec.Managed.TokenSecretRef.Name) == obj.GetName() {
			requests = append(requests, reconcile.Request{NamespacedName: client.ObjectKeyFromObject(model)})
		}
	}
	return requests
}
