package tests

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"istio.io/istio/pkg/config/crd"
	"istio.io/istio/pkg/lazy"
	"istio.io/istio/pkg/test/util/assert"
	"k8s.io/apiextensions-apiserver/pkg/apis/apiextensions"
	apiextensionsv1 "k8s.io/apiextensions-apiserver/pkg/apis/apiextensions/v1"
	apiextensionsv1beta1 "k8s.io/apiextensions-apiserver/pkg/apis/apiextensions/v1beta1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	kubeyaml "k8s.io/apimachinery/pkg/util/yaml"

	"github.com/agentgateway/agentgateway/controller/pkg/utils/fsutils"
)

type Validator struct {
	*crd.Validator
	schemas map[schema.GroupVersionKind]*apiextensions.JSONSchemaProps
}

var validator = lazy.New(func() (*Validator, error) {
	return newAgentgatewayValidator(false)
})
var validatorSkipMissing = lazy.New(func() (*Validator, error) {
	return newAgentgatewayValidator(true)
})

func NewAgentgatewayValidator(t *testing.T) *Validator {
	v, err := validator.Get()
	assert.NoError(t, err)
	return v
}

func NewAgentgatewayValidatorSkipMissing(t *testing.T) *Validator {
	v, err := validatorSkipMissing.Get()
	assert.NoError(t, err)
	return v
}

func newAgentgatewayValidator(skipMissing bool) (*Validator, error) {
	root := fsutils.GetModuleRoot()
	dirs := []string{}
	agentgatewayDir, err := os.ReadDir(filepath.Join(root, "controller/install/helm/agentgateway-crds/templates/"))
	if err != nil {
		return nil, err
	}
	for _, d := range agentgatewayDir {
		if strings.HasSuffix(d.Name(), ".yaml") {
			dirs = append(dirs, filepath.Join(root, "controller/install/helm/agentgateway-crds/templates", d.Name()))
		}
	}
	v, err := crd.NewValidatorFromFiles(
		dirs...,
	)
	if err != nil {
		return nil, err
	}
	schemas, err := schemasFromFiles(dirs...)
	if err != nil {
		return nil, err
	}
	v.SkipMissing = skipMissing
	return &Validator{
		Validator: v,
		schemas:   schemas,
	}, nil
}

func schemasFromFiles(files ...string) (map[schema.GroupVersionKind]*apiextensions.JSONSchemaProps, error) {
	crds := []apiextensions.CustomResourceDefinition{}
	closers := make([]io.Closer, 0, len(files))
	defer func() {
		for _, closer := range closers {
			closer.Close()
		}
	}()
	for _, file := range files {
		data, err := os.Open(file)
		if err != nil {
			return nil, fmt.Errorf("failed to read input yaml file: %v", err)
		}
		closers = append(closers, data)

		yamlDecoder := kubeyaml.NewYAMLOrJSONDecoder(data, 512*1024)
		for {
			un := &unstructured.Unstructured{}
			err = yamlDecoder.Decode(un)
			if err == io.EOF {
				break
			}
			if err != nil {
				return nil, err
			}
			crd := apiextensions.CustomResourceDefinition{}
			switch un.GroupVersionKind() {
			case schema.GroupVersionKind{
				Group:   "apiextensions.k8s.io",
				Version: "v1",
				Kind:    "CustomResourceDefinition",
			}:
				crdv1 := apiextensionsv1.CustomResourceDefinition{}
				if err := runtime.DefaultUnstructuredConverter.
					FromUnstructured(un.UnstructuredContent(), &crdv1); err != nil {
					return nil, err
				}
				if err := apiextensionsv1.Convert_v1_CustomResourceDefinition_To_apiextensions_CustomResourceDefinition(&crdv1, &crd, nil); err != nil {
					return nil, err
				}
			case schema.GroupVersionKind{
				Group:   "apiextensions.k8s.io",
				Version: "v1beta1",
				Kind:    "CustomResourceDefinition",
			}:
				crdv1beta1 := apiextensionsv1beta1.CustomResourceDefinition{}
				if err := runtime.DefaultUnstructuredConverter.
					FromUnstructured(un.UnstructuredContent(), &crdv1beta1); err != nil {
					return nil, err
				}
				if err := apiextensionsv1beta1.Convert_v1beta1_CustomResourceDefinition_To_apiextensions_CustomResourceDefinition(&crdv1beta1, &crd, nil); err != nil {
					return nil, err
				}
			case schema.GroupVersionKind{}:
				continue
			default:
				return nil, fmt.Errorf("unknown CRD type: %v", un.GroupVersionKind())
			}
			crds = append(crds, crd)
		}
	}

	res := map[schema.GroupVersionKind]*apiextensions.JSONSchemaProps{}
	for _, crd := range crds {
		versions := crd.Spec.Versions
		if len(versions) == 0 {
			versions = []apiextensions.CustomResourceDefinitionVersion{{Name: crd.Spec.Version}} // nolint:staticcheck
		}
		for _, ver := range versions {
			crdSchema := ver.Schema
			if crdSchema == nil {
				crdSchema = crd.Spec.Validation
			}
			if crdSchema == nil {
				return nil, fmt.Errorf("crd did not have validation defined")
			}
			gvk := schema.GroupVersionKind{
				Group:   crd.Spec.Group,
				Version: ver.Name,
				Kind:    crd.Spec.Names.Kind,
			}
			res[gvk] = crdSchema.OpenAPIV3Schema
		}
	}
	return res, nil
}
