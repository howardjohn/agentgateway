package tests

import (
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"testing"

	"istio.io/istio/pkg/test/util/assert"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"sigs.k8s.io/yaml"
)

// Split where the '---' appears at the very beginning of a line. This will avoid
// accidentally splitting in cases where yaml resources contain nested yaml (which
// is indented).
var splitRegex = regexp.MustCompile(`(^|\n)---`)

// SplitString splits the given yaml doc if it's multipart document.
func splitString(yamlText string) []string {
	out := make([]string, 0)
	parts := splitRegex.Split(yamlText, -1)
	for _, part := range parts {
		part := strings.TrimSpace(part)
		if len(part) > 0 {
			out = append(out, part)
		}
	}
	return out
}

type TestExpectation struct {
	WantErr string `json:"_err,omitempty"`
}

func TestCRDs(t *testing.T) {
	v := NewAgentgatewayValidator(t)
	base := "testdata"
	d, err := os.ReadDir(base)
	if err != nil {
		t.Fatal(err)
	}
	for _, f := range d {
		t.Run(f.Name(), func(t *testing.T) {
			f, err := os.ReadFile(filepath.Join("testdata", f.Name()))
			if err != nil {
				t.Fatal(err)
			}
			for _, item := range splitString(string(f)) {
				obj := &unstructured.Unstructured{}
				if err := yaml.Unmarshal([]byte(item), obj); err != nil {
					t.Fatal(err)
				}
				delete(obj.Object, "_err")
				t.Run(obj.GetName(), func(t *testing.T) {
					want := TestExpectation{}
					if err := yaml.Unmarshal([]byte(item), &want); err != nil {
						t.Fatal(err)
					}
					res := v.ValidateCustomResource(obj)
					if want.WantErr == "" {
						// Want no error
						if res != nil {
							t.Fatalf("configuration was invalid: %v", res)
						}
					} else {
						if res == nil {
							t.Fatalf("wanted error like %q, got none", want.WantErr)
						}
						if !strings.Contains(res.Error(), want.WantErr) {
							t.Fatalf("wanted error like %q, got %q", want.WantErr, res)
						}
					}
				})
			}
		})
	}
}

func TestCRDCosts(t *testing.T) {
	v := NewAgentgatewayValidator(t)

	d, writeReports := os.LookupEnv("CRD_REPORT_DIRECTORY")
	if writeReports {
		assert.NoError(t, os.MkdirAll(d, 0o777))
	}

	gvks := make([]string, 0, len(v.schemas))
	for gvk := range v.schemas {
		gvks = append(gvks, gvk.String())
	}
	sort.Strings(gvks)
	for _, gvkString := range gvks {
		t.Run(gvkString, func(t *testing.T) {
			for gvk := range v.schemas {
				if gvk.String() != gvkString {
					continue
				}
				cr, err := v.ValidateCosts(gvk)
				assert.NoError(t, err)
				if writeReports {
					report := cr.MarkdownReport(gvk.String())
					assert.NoError(t, os.WriteFile(filepath.Join(d, strings.ReplaceAll(gvkString, "/", "_")+".md"), []byte(report), 0o777))
				}
				return
			}
		})
	}
}
