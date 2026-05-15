package tests

import (
	"errors"
	"fmt"
	"math"
	"sort"
	"strings"
	"sync"

	"golang.org/x/text/language"
	"golang.org/x/text/message"
	"k8s.io/apiextensions-apiserver/pkg/apis/apiextensions"
	apiextval "k8s.io/apiextensions-apiserver/pkg/apis/apiextensions/validation"
	structuralschema "k8s.io/apiextensions-apiserver/pkg/apiserver/schema"
	"k8s.io/apiextensions-apiserver/pkg/apiserver/schema/cel"
	"k8s.io/apiextensions-apiserver/pkg/apiserver/schema/cel/model"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/util/validation/field"
	celconfig "k8s.io/apiserver/pkg/apis/cel"
	apiservercel "k8s.io/apiserver/pkg/cel"
	"k8s.io/apiserver/pkg/cel/environment"
)

type CostReports struct {
	Expressions  []ExpressionReport
	Total        uint64
	TotalAllowed uint64
}

func (c CostReports) UsedBudget() float64 {
	return float64(c.Total) / float64(c.TotalAllowed)
}

func (c CostReports) MarkdownReport(title string) string {
	p := message.NewPrinter(language.English)
	sb := strings.Builder{}
	sb.WriteString(" ## " + title + "\n\n")
	sb.WriteString(p.Sprintf("**Total cost:** %d (%.2f%%)\n\n", c.Total, c.UsedBudget()*100))
	sb.WriteString("|Path|Expression Cost|Path Cost|Budget|Cardinality|Cumulative|Expression|\n")
	sb.WriteString("|-|-|-|-|-|-|-|\n")
	cum := uint64(0)
	for _, e := range c.Expressions {
		cum += e.Cost
		sb.WriteString(p.Sprintf("|`%s`|%d|%d|%.2f%%|%d|%.2f%%|`%s`|\n",
			strings.ReplaceAll(e.Path.String(), "^.spec.", ""),
			e.RawCost,
			e.Cost,
			e.UsedBudget()*100,
			e.Cardinality,
			float64(cum)/float64(c.TotalAllowed)*100,
			strings.ReplaceAll(e.Rule.Rule, "|", "\\|"),
		))
	}
	return sb.String()
}

type ExpressionReport struct {
	Rule        apiextensions.ValidationRule
	RawCost     uint64
	Cost        uint64
	Allowed     uint64
	Cardinality uint64
	Path        *field.Path
}

func (c ExpressionReport) UsedBudget() float64 {
	return float64(c.Cost) / float64(c.Allowed)
}

func (v *Validator) ValidateCosts(gvk schema.GroupVersionKind) (CostReports, error) {
	s, f := v.schemas[gvk]
	if !f {
		return CostReports{}, fmt.Errorf("unknown schema: %v", gvk)
	}
	cb, err := validateCosts(s)
	if err != nil {
		return CostReports{}, err
	}
	sort.Slice(cb.Expressions, func(i, j int) bool {
		return cb.Expressions[i].Cost > cb.Expressions[j].Cost
	})
	return cb, nil
}

func validateCosts(schema *apiextensions.JSONSchemaProps) (CostReports, error) {
	errsToReport := []string{}
	warnings := []string{}
	infos := []string{}
	res := CostReports{}

	rootCELContext := apiextval.RootCELContext(schema)

	schemaHas(schema, field.NewPath("^"), field.NewPath("^"), nil,
		func(schema *apiextensions.JSONSchemaProps, fldPath, simpleLocation *field.Path, ancestry []*apiextensions.JSONSchemaProps) bool {
			if schema.XValidations == nil {
				return false
			}

			schemaInfos, schemaWarnings, err := inspectSchema(schema, simpleLocation, len(ancestry) == 0)
			if err != nil {
				errsToReport = append(errsToReport, err.Error())
			}
			infos = append(infos, schemaInfos...)
			warnings = append(warnings, schemaWarnings...)

			celContext := extractCELContext(append(ancestry, schema))

			typeInfo, err := celContext.TypeInfo()
			if err != nil {
				errsToReport = append(errsToReport, err.Error())
				return false
			}

			if typeInfo == nil {
				return false
			}

			compResults, err := cel.Compile(
				typeInfo.Schema,
				typeInfo.DeclType,
				celconfig.PerCallLimit,
				environment.MustBaseEnvSet(environment.DefaultCompatibilityVersion()),
				cel.NewExpressionsEnvLoader(),
			)
			if err != nil {
				fieldErr := field.InternalError(fldPath, fmt.Errorf("failed to compile x-kubernetes-validations rules: %w", err))
				errsToReport = append(errsToReport, fieldErr.Error())
				return false
			}

			for i, cr := range compResults {
				if celContext.MaxCardinality == nil {
					unboundedParents, err := getUnboundedParentFields(ancestry, fldPath)
					if err != nil {
						errsToReport = append(errsToReport, err.Error())
					}
					warnings = append(warnings,
						fmt.Sprintf(
							"%s: Field has unbounded cardinality. At least one, variable parent field does not have a maxItems or maxProperties constraint: %s."+
								"Falling back to CEL calculated worst case of %d executions.",
							simpleLocation.String(), strings.Join(unboundedParents, ","), cr.MaxCardinality))
				} else {
					msg := fmt.Sprintf("%s: Field has a maximum cardinality of %d.", simpleLocation.String(), *celContext.MaxCardinality)
					if *celContext.MaxCardinality > 1 {
						msg += " This is the calculated, worst case number of times the rule will be evaluated."
					}

					infos = append(infos, msg)
				}

				expressionCost := getExpressionCost(cr, celContext)
				res.Expressions = append(res.Expressions, ExpressionReport{
					Rule:        schema.XValidations[i],
					RawCost:     cr.MaxCost,
					Cost:        expressionCost,
					Cardinality: orDefault(celContext.MaxCardinality, 0),
					Allowed:     apiextval.StaticEstimatedCostLimit,
					Path:        simpleLocation,
				})

				if expressionCost > apiextval.StaticEstimatedCostLimit {
					costErrorMsg := getCostErrorMessage("estimated rule cost", expressionCost, apiextval.StaticEstimatedCostLimit)
					errsToReport = append(errsToReport, field.Forbidden(fldPath, costErrorMsg).Error())
				}
				if rootCELContext.TotalCost != nil {
					rootCELContext.TotalCost.ObserveExpressionCost(fldPath, expressionCost)
				}

				if cr.Error != nil {
					if cr.Error.Type == apiservercel.ErrorTypeRequired {
						errsToReport = append(errsToReport, field.Required(fldPath, cr.Error.Detail).Error())
					} else {
						errsToReport = append(errsToReport, field.Invalid(fldPath, schema.XValidations[i], cr.Error.Detail).Error())
					}
				} else {
					infos = append(infos,
						fmt.Sprintf("%s: Rule %d raw cost is %d. Estimated total cost of %d. The maximum allowable value is %d. Rule is %.2f%% of allowed budget.",
							simpleLocation.String(), i, cr.MaxCost, expressionCost,
							apiextval.StaticEstimatedCostLimit, float64(expressionCost*100)/apiextval.StaticEstimatedCostLimit))
				}

				if cr.MessageExpressionError != nil {
					errsToReport = append(errsToReport, field.Invalid(fldPath, schema.XValidations[i], cr.MessageExpressionError.Detail).Error())
				} else if cr.MessageExpression != nil {
					if cr.MessageExpressionMaxCost > apiextval.StaticEstimatedCostLimit {
						costErrorMsg := getCostErrorMessage("estimated messageExpression cost", cr.MessageExpressionMaxCost, apiextval.StaticEstimatedCostLimit)
						errsToReport = append(errsToReport, field.Forbidden(fldPath, costErrorMsg).Error())
					}
					if celContext.TotalCost != nil {
						celContext.TotalCost.ObserveExpressionCost(fldPath, cr.MessageExpressionMaxCost)
					}
				}
			}

			return false
		})

	if rootCELContext != nil && rootCELContext.TotalCost != nil && rootCELContext.TotalCost.Total > apiextval.StaticEstimatedCRDCostLimit {
		costErrorMsg := getCostErrorMessage("total CRD cost", rootCELContext.TotalCost.Total, apiextval.StaticEstimatedCRDCostLimit)
		errsToReport = append(errsToReport, field.Forbidden(field.NewPath("^"), costErrorMsg).Error())
	}

	res.Total = rootCELContext.TotalCost.Total
	res.TotalAllowed = apiextval.StaticEstimatedCRDCostLimit
	if len(errsToReport) > 0 {
		return res, fmt.Errorf("%s", strings.Join(errsToReport, "; "))
	}
	return res, nil
}

func getCostErrorMessage(costName string, expressionCost, costLimit uint64) string {
	exceedFactor := float64(expressionCost) / float64(costLimit)
	var factor string
	if exceedFactor > 100.0 {
		factor = "more than 100x"
	} else if exceedFactor < 1.5 {
		factor = fmt.Sprintf("%fx", exceedFactor)
	} else {
		factor = fmt.Sprintf("%.1fx", exceedFactor)
	}
	return fmt.Sprintf(
		"%s exceeds budget by factor of %s (try simplifying the rule(s), or adding maxItems,"+
			" maxProperties, and maxLength where arrays, maps, and strings are declared)", costName, factor)
}

func getExpressionCost(cr cel.CompilationResult, cardinalityCost *apiextval.CELSchemaContext) uint64 {
	if cardinalityCost.MaxCardinality != nil {
		return multiplyWithOverflowGuard(cr.MaxCost, *cardinalityCost.MaxCardinality)
	}
	return multiplyWithOverflowGuard(cr.MaxCost, cr.MaxCardinality)
}

func multiplyWithOverflowGuard(baseCost, cardinality uint64) uint64 {
	if baseCost == 0 {
		return 0
	} else if math.MaxUint/baseCost < cardinality {
		return math.MaxUint
	}
	return baseCost * cardinality
}

type schemaWalkerFunc func(s *apiextensions.JSONSchemaProps, fldPath, simpleLocation *field.Path, ancestry []*apiextensions.JSONSchemaProps) bool

func schemaHas(
	s *apiextensions.JSONSchemaProps,
	fldPath, simpleLocation *field.Path,
	ancestry []*apiextensions.JSONSchemaProps,
	pred schemaWalkerFunc,
) bool {
	if s == nil {
		return false
	}

	if pred(s, fldPath, simpleLocation, ancestry) {
		return true
	}

	nextAncestry := append(ancestry, s)

	if s.Items != nil {
		if s.Items != nil && schemaHasRecurse(s.Items.Schema, fldPath.Child("items"), simpleLocation.Key("*"), nextAncestry, pred) {
			return true
		}
		for i := range s.Items.JSONSchemas {
			if schemaHasRecurse(
				&s.Items.JSONSchemas[i],
				fldPath.Child("items", "jsonSchemas").Index(i),
				simpleLocation.Index(i),
				nextAncestry,
				pred,
			) {
				return true
			}
		}
	}
	for i := range s.AllOf {
		if schemaHasRecurse(&s.AllOf[i], fldPath.Child("allOf").Index(i), simpleLocation, nextAncestry, pred) {
			return true
		}
	}
	for i := range s.AnyOf {
		if schemaHasRecurse(&s.AnyOf[i], fldPath.Child("anyOf").Index(i), simpleLocation, nextAncestry, pred) {
			return true
		}
	}
	for i := range s.OneOf {
		if schemaHasRecurse(&s.OneOf[i], fldPath.Child("oneOf").Index(i), simpleLocation, nextAncestry, pred) {
			return true
		}
	}
	if schemaHasRecurse(s.Not, fldPath.Child("not"), simpleLocation, nextAncestry, pred) {
		return true
	}
	for propertyName, s := range s.Properties {
		if schemaHasRecurse(&s, fldPath.Child("properties").Key(propertyName), simpleLocation.Child(propertyName), nextAncestry, pred) {
			return true
		}
	}
	if s.AdditionalProperties != nil {
		if schemaHasRecurse(s.AdditionalProperties.Schema, fldPath.Child("additionalProperties", "schema"), simpleLocation.Key("*"), nextAncestry, pred) {
			return true
		}
	}
	for patternName, s := range s.PatternProperties {
		if schemaHasRecurse(&s, fldPath.Child("allOf").Key(patternName), simpleLocation, nextAncestry, pred) {
			return true
		}
	}
	if s.AdditionalItems != nil {
		if schemaHasRecurse(s.AdditionalItems.Schema, fldPath.Child("additionalItems", "schema"), simpleLocation, nextAncestry, pred) {
			return true
		}
	}
	for _, s := range s.Definitions {
		if schemaHasRecurse(&s, fldPath.Child("definitions"), simpleLocation, nextAncestry, pred) {
			return true
		}
	}
	for dependencyName, d := range s.Dependencies {
		if schemaHasRecurse(d.Schema, fldPath.Child("dependencies").Key(dependencyName).Child("schema"), simpleLocation, nextAncestry, pred) {
			return true
		}
	}

	return false
}

var schemaPool = sync.Pool{
	New: func() any {
		return new(apiextensions.JSONSchemaProps)
	},
}

func schemaHasRecurse(
	s *apiextensions.JSONSchemaProps,
	fldPath, simpleLocation *field.Path,
	ancestry []*apiextensions.JSONSchemaProps,
	pred schemaWalkerFunc,
) bool {
	if s == nil {
		return false
	}
	schema := schemaPool.Get().(*apiextensions.JSONSchemaProps)
	defer schemaPool.Put(schema)
	*schema = *s
	return schemaHas(schema, fldPath, simpleLocation, ancestry, pred)
}

func inspectSchema(schema *apiextensions.JSONSchemaProps, simpleLocation *field.Path, isRoot bool) ([]string, []string, error) {
	typeInfo, err := getDeclType(schema, isRoot)
	if err != nil {
		return nil, nil, err
	}

	var infos, warnings []string

	switch schema.Type {
	case "string":
		switch {
		case len(schema.Enum) > 0:
		case schema.MaxLength == nil:
			warnings = append(warnings,
				fmt.Sprintf("%s: String has unbounded maxLength. It will be considered to have length %d."+
					" Consider adding a maxLength constraint to reduce the raw rule cost.",
					simpleLocation.String(), typeInfo.MaxElements))
		default:
			infos = append(infos, fmt.Sprintf("%s: String has maxLength of %d.", simpleLocation.String(), *schema.MaxLength))
		}
	case "array":
		switch {
		case schema.MaxItems == nil:
			warnings = append(warnings,
				fmt.Sprintf("%s: Array has unbounded maxItems. It will be considered to have %d items."+
					" Consider adding a maxItems constraint to reduce the raw rule cost.",
					simpleLocation.String(), typeInfo.MaxElements))
		default:
			infos = append(infos, fmt.Sprintf("%s: Array has maxItems of %d.", simpleLocation.String(), *schema.MaxItems))
		}
	}

	return infos, warnings, nil
}

func getDeclType(schema *apiextensions.JSONSchemaProps, isRoot bool) (*apiservercel.DeclType, error) {
	structural, err := structuralschema.NewStructural(schema)
	if err != nil {
		return nil, err
	}
	declType := model.SchemaDeclType(structural, isRoot)
	if declType == nil {
		return nil, fmt.Errorf("unable to convert structural schema to CEL declarations")
	}
	return declType, nil
}

func extractCELContext(schemas []*apiextensions.JSONSchemaProps) *apiextval.CELSchemaContext {
	var celContext *apiextval.CELSchemaContext

	for _, s := range schemas {
		if celContext == nil {
			celContext = apiextval.RootCELContext(s)
			continue
		}

		celContext = celContext.ChildPropertyContext(s, s.ID)
	}

	return celContext
}

func getUnboundedParentFields(ancestry []*apiextensions.JSONSchemaProps, fldPath *field.Path) ([]string, error) {
	cleanPathParts := getCleanPathParts(fldPath)
	var path *field.Path

	if len(ancestry)+1 != len(cleanPathParts) {
		return nil, errors.New("ancestry and field path do not match")
	}

	unboundedParents := []string{}
	for i, schema := range ancestry {
		if path == nil {
			path = field.NewPath(cleanPathParts[i])
		} else if cleanPathParts[i] == "items" {
			path = path.Index(-1)
		} else {
			path = path.Child(cleanPathParts[i])
		}

		if isUnboundedCardinality(schema) {
			unboundedParents = append(unboundedParents, strings.Replace(path.String(), "-1", "*", -1))
		}
	}
	return unboundedParents, nil
}

func isUnboundedCardinality(schema *apiextensions.JSONSchemaProps) bool {
	switch schema.Type {
	case "object":
		return schema.AdditionalProperties != nil && schema.MaxProperties == nil
	case "array":
		return schema.MaxItems == nil
	default:
		return false
	}
}

func getCleanPathParts(fldPath *field.Path) []string {
	cleanPathParts := []string{}
	for _, part := range strings.Split(fldPath.String(), ".") {
		if strings.HasPrefix(part, "properties[") {
			part = strings.TrimPrefix(strings.TrimSuffix(part, "]"), "properties[")
		}
		cleanPathParts = append(cleanPathParts, part)
	}
	return cleanPathParts
}

func orDefault[T any](t *T, def T) T {
	if t == nil {
		return def
	}
	return *t
}
